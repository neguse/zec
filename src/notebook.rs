//! Terminal projection for Zed's native notebook editor.
//!
//! `NotebookEditor` owns nbformat cells, their Zed editors, kernel lifecycle,
//! and Jupyter message routing.  This module only keeps terminal presentation
//! state and renders immutable `NotebookEditor::to_notebook` snapshots. Existing rich
//! outputs that upstream cannot serialize are retained by cell ID until an action invalidates
//! them, preventing a terminal edit from erasing notebook data.

use std::{collections::BTreeMap, ffi::OsStr, path::Path};

use anyhow::{Context as _, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use editor::actions::HandleInput;
use gpui::{
    Action, App, AppContext as _, Entity, Focusable as _, Keystroke, WindowBounds, WindowHandle,
    WindowOptions,
};
use language::Buffer;
use project::{Project, ProjectItem as _};
use ratatui::{
    buffer::Buffer as TerminalBuffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::Widget,
};
use repl::notebook::{NotebookEditor, NotebookItem};
use serde_json::Value;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind};
use zed_actions::{
    editor::{MoveDown, MoveUp},
    notebook::{
        AddCodeBlock, AddMarkdownBlock, ClearOutputs, DeleteCell, EnterCommandMode, EnterEditMode,
        InterruptKernel, MoveCellDown, MoveCellUp, RestartKernel, Run, RunAll, RunAndAdvance,
    },
};

use crate::{input, terminal::TerminalEvent};

const MAX_NOTEBOOK_CELLS: usize = 10_000;
const MAX_SOURCE_LINES_PER_CELL: usize = 10_000;
const MAX_OUTPUT_LINES_PER_CELL: usize = 10_000;
const MAX_PROJECTED_LINE_BYTES: usize = 64 * 1024;

#[derive(Debug)]
struct NotebookKernelProcesses {
    owner_pid: Option<sysinfo::Pid>,
    connection_file_name: String,
}

impl NotebookKernelProcesses {
    fn new(entity_id: u64) -> Self {
        Self {
            owner_pid: sysinfo::get_current_pid().ok(),
            connection_file_name: format!("kernel-zed-{entity_id}.json"),
        }
    }

    fn matching_pids(&self) -> Vec<sysinfo::Pid> {
        let Some(owner_pid) = self.owner_pid else {
            return Vec::new();
        };
        let system = notebook_process_snapshot();
        system
            .processes()
            .iter()
            .filter_map(|(pid, process)| {
                (process_uses_connection_file(process, &self.connection_file_name)
                    && process_is_descendant_of(&system, *pid, owner_pid))
                .then_some(*pid)
            })
            .collect()
    }

    fn terminate_pids(&self, requested: Option<&[sysinfo::Pid]>) -> usize {
        let Some(owner_pid) = self.owner_pid else {
            return 0;
        };
        let system = notebook_process_snapshot();
        let candidates = system
            .processes()
            .iter()
            .filter_map(|(pid, process)| {
                let requested = requested.is_none_or(|requested| requested.contains(pid));
                (requested
                    && process_uses_connection_file(process, &self.connection_file_name)
                    && process_is_descendant_of(&system, *pid, owner_pid))
                .then_some(*pid)
            })
            .collect::<Vec<_>>();

        candidates
            .into_iter()
            .filter(|pid| {
                system
                    .process(*pid)
                    .is_some_and(|process| terminate_kernel_process(*pid, process))
            })
            .count()
    }
}

impl Drop for NotebookKernelProcesses {
    fn drop(&mut self) {
        let _ = self.terminate_pids(None);
    }
}

fn notebook_process_snapshot() -> System {
    let refresh = ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always);
    let mut system = System::new_with_specifics(RefreshKind::nothing().with_processes(refresh));
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, refresh);
    system
}

fn process_uses_connection_file(process: &sysinfo::Process, connection_file_name: &str) -> bool {
    let connection_file_name = OsStr::new(connection_file_name);
    process
        .cmd()
        .iter()
        .any(|argument| is_connection_file_argument(argument, connection_file_name))
}

fn is_connection_file_argument(argument: &OsStr, connection_file_name: &OsStr) -> bool {
    if argument == connection_file_name {
        return true;
    }
    if argument.to_string_lossy().starts_with('-') {
        return false;
    }
    Path::new(argument).file_name() == Some(connection_file_name)
}

fn process_is_descendant_of(system: &System, pid: sysinfo::Pid, owner: sysinfo::Pid) -> bool {
    let mut cursor = Some(pid);
    for _ in 0..=system.processes().len() {
        let Some(pid) = cursor else {
            return false;
        };
        if pid == owner {
            return true;
        }
        cursor = system.process(pid).and_then(sysinfo::Process::parent);
    }
    false
}

#[cfg(unix)]
fn terminate_kernel_process(pid: sysinfo::Pid, process: &sysinfo::Process) -> bool {
    let group_terminated = i32::try_from(pid.as_u32()).ok().is_some_and(|pid| {
        nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        )
        .is_ok()
    });
    group_terminated || process.kill()
}

#[cfg(not(unix))]
fn terminate_kernel_process(_pid: sysinfo::Pid, process: &sysinfo::Process) -> bool {
    process.kill()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NotebookMode {
    Command,
    Edit,
}

impl NotebookMode {
    fn label(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Edit => "edit",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NotebookRowKind {
    Title,
    CellHeader,
    Source,
    Output,
    Error,
    Hint,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NotebookRow {
    text: String,
    selected: bool,
    kind: NotebookRowKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NotebookSnapshot {
    rows: Vec<NotebookRow>,
    scroll: usize,
    status: String,
}

pub(crate) struct NotebookWidget<'a> {
    snapshot: &'a NotebookSnapshot,
}

impl<'a> NotebookWidget<'a> {
    pub(crate) fn new(snapshot: &'a NotebookSnapshot) -> Self {
        Self { snapshot }
    }
}

impl Widget for NotebookWidget<'_> {
    fn render(self, area: Rect, buffer: &mut TerminalBuffer) {
        if area.is_empty() {
            return;
        }
        buffer.set_style(area, Style::default().bg(Color::Black).fg(Color::White));
        let body_height = usize::from(area.height.saturating_sub(1));
        let width = usize::from(area.width);
        for (screen_row, row) in self
            .snapshot
            .rows
            .iter()
            .skip(self.snapshot.scroll)
            .take(body_height)
            .enumerate()
        {
            let style = row_style(row);
            let y = area
                .y
                .saturating_add(u16::try_from(screen_row).unwrap_or(u16::MAX));
            buffer.set_style(Rect::new(area.x, y, area.width, 1), style);
            buffer.set_stringn(area.x, y, &row.text, width, style);
        }

        if area.height > 0 {
            let y = area.y.saturating_add(area.height - 1);
            let style = Style::default().bg(Color::DarkGray).fg(Color::White);
            buffer.set_style(Rect::new(area.x, y, area.width, 1), style);
            buffer.set_stringn(area.x, y, &self.snapshot.status, width, style);
        }
    }
}

fn row_style(row: &NotebookRow) -> Style {
    let mut style = match row.kind {
        NotebookRowKind::Title => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        NotebookRowKind::CellHeader => Style::default().fg(Color::Yellow),
        NotebookRowKind::Source => Style::default().fg(Color::White),
        NotebookRowKind::Output => Style::default().fg(Color::Green),
        NotebookRowKind::Error => Style::default().fg(Color::Red),
        NotebookRowKind::Hint => Style::default().fg(Color::DarkGray),
    };
    if row.selected {
        style = style.bg(Color::DarkGray);
    }
    style
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum NotebookInput {
    Consumed,
    Unhandled,
}

pub(crate) struct NotebookState {
    window: WindowHandle<NotebookEditor>,
    kernel_processes: NotebookKernelProcesses,
    mode: NotebookMode,
    selected_cell: usize,
    cell_count: usize,
    scroll: usize,
    delete_armed: bool,
    last_authority_json: String,
    preserved_outputs: BTreeMap<String, Vec<Value>>,
    notice: String,
}

impl NotebookState {
    pub(crate) async fn open(
        buffer: Entity<Buffer>,
        project: Entity<Project>,
        redraw_sender: async_channel::Sender<TerminalEvent>,
        cx: &mut gpui::AsyncApp,
    ) -> Result<Option<Self>> {
        let Some(project_path) = buffer.read_with(cx, |buffer, cx| buffer.project_path(cx)) else {
            return Ok(None);
        };
        if project_path.path.extension().unwrap_or_default() != "ipynb" {
            return Ok(None);
        }

        let raw_notebook = buffer.read_with(cx, |buffer, _cx| buffer.text());
        let raw_notebook = serde_json::from_str(&raw_notebook).unwrap_or(Value::Null);

        let open_task = cx
            .update(|cx| NotebookItem::try_open(&project, &project_path, cx))
            .context("Zed did not recognize the .ipynb project item")?;
        let notebook_item = open_task.await.context("open Zed NotebookItem")?;
        let notebook_project = project.clone();
        let window = cx.update(|cx| open_notebook_window(notebook_project, notebook_item, cx))?;
        let kernel_processes = NotebookKernelProcesses::new(
            window
                .entity(cx)
                .context("read hidden Zed NotebookEditor entity")?
                .entity_id()
                .as_u64(),
        );
        window
            .update(cx, |_notebook, _window, cx| {
                cx.observe_self(move |_notebook, _cx| {
                    let _ = redraw_sender.try_send(TerminalEvent::Redraw);
                })
                .detach();
            })
            .context("observe Zed NotebookEditor updates")?;

        let mut value = notebook_value(&window, cx)?;
        let preserved_outputs = collect_preserved_outputs(&raw_notebook, &value);
        merge_preserved_outputs(&mut value, &preserved_outputs);
        let last_authority_json = serde_json::to_string_pretty(&value)
            .context("serialize initial Zed notebook snapshot")?;
        let cell_count = notebook_cells(&value).len();
        Ok(Some(Self {
            window,
            kernel_processes,
            mode: NotebookMode::Command,
            selected_cell: 0,
            cell_count,
            scroll: 0,
            delete_armed: false,
            last_authority_json,
            preserved_outputs,
            notice: "Zed kernel starting; missing kernels are reported in the selected cell"
                .to_owned(),
        }))
    }

    pub(crate) fn synchronize(
        &mut self,
        raw_buffer: &Entity<Buffer>,
        cx: &mut gpui::AsyncApp,
    ) -> Result<bool> {
        let mut value = notebook_value(&self.window, cx)?;
        merge_preserved_outputs(&mut value, &self.preserved_outputs);
        self.cell_count = notebook_cells(&value).len();
        self.selected_cell = self.selected_cell.min(self.cell_count.saturating_sub(1));
        let serialized =
            serde_json::to_string_pretty(&value).context("serialize Zed notebook authority")?;
        if serialized == self.last_authority_json {
            return Ok(false);
        }

        let replacement = serialized.clone();
        raw_buffer.update(cx, |buffer, cx| {
            if buffer.text() != replacement {
                buffer.set_text(replacement, cx);
            }
        });
        self.last_authority_json = serialized;
        Ok(true)
    }

    pub(crate) fn snapshot(
        &mut self,
        width: usize,
        height: usize,
        message: Option<&str>,
        cx: &mut gpui::AsyncApp,
    ) -> Result<NotebookSnapshot> {
        let mut value = notebook_value(&self.window, cx)?;
        merge_preserved_outputs(&mut value, &self.preserved_outputs);
        let cells = notebook_cells(&value);
        self.cell_count = cells.len();
        self.selected_cell = self.selected_cell.min(self.cell_count.saturating_sub(1));
        let mut snapshot = snapshot_from_value(
            &value,
            self.selected_cell,
            self.mode,
            self.scroll,
            width,
            height,
            &self.notice,
        );
        self.scroll = snapshot.scroll;
        if let Some(message) = message {
            snapshot.status = format!("{message}  |  {}", snapshot.status);
        }
        Ok(snapshot)
    }

    pub(crate) fn handle_key(
        &mut self,
        event: KeyEvent,
        body_height: usize,
        cx: &mut gpui::AsyncApp,
    ) -> Result<NotebookInput> {
        if event.kind == KeyEventKind::Release {
            return Ok(NotebookInput::Consumed);
        }
        if is_workspace_shortcut(&event) {
            self.delete_armed = false;
            return Ok(NotebookInput::Unhandled);
        }

        if is_run(&event) {
            self.forget_selected_outputs(cx)?;
            self.dispatch_action(Box::new(Run), cx)?;
            self.delete_armed = false;
            self.notice = "cell submitted to the Zed Jupyter session".to_owned();
            return Ok(NotebookInput::Consumed);
        }
        if is_run_and_advance(&event) {
            self.forget_selected_outputs(cx)?;
            self.dispatch_action(Box::new(RunAndAdvance), cx)?;
            self.selected_cell = self.selected_cell.saturating_add(1);
            self.mode = NotebookMode::Command;
            self.delete_armed = false;
            self.notice = "cell submitted; advanced in command mode".to_owned();
            return Ok(NotebookInput::Consumed);
        }

        match self.mode {
            NotebookMode::Edit => {
                if event.code == KeyCode::Esc && event.modifiers.is_empty() {
                    self.dispatch_action(Box::new(EnterCommandMode), cx)?;
                    self.mode = NotebookMode::Command;
                    self.delete_armed = false;
                    self.notice = "notebook command mode".to_owned();
                } else if let Some(keystroke) = input::to_gpui_keystroke(event) {
                    if let Some(text) = keystroke.key_char.clone() {
                        self.dispatch_text(text, cx)?;
                    } else {
                        self.dispatch_keystroke(keystroke, cx)?;
                    }
                    self.delete_armed = false;
                    self.notice = "editing selected cell with Zed Editor".to_owned();
                }
                Ok(NotebookInput::Consumed)
            }
            NotebookMode::Command => {
                self.handle_command_key(event, body_height, cx)?;
                Ok(NotebookInput::Consumed)
            }
        }
    }

    pub(crate) fn handle_paste(
        &mut self,
        text: &str,
        cx: &mut gpui::AsyncApp,
    ) -> Result<NotebookInput> {
        if self.mode != NotebookMode::Edit {
            self.notice = "paste ignored in command mode; press Enter to edit the cell".to_owned();
            return Ok(NotebookInput::Consumed);
        }
        self.dispatch_text(text.to_owned(), cx)?;
        self.delete_armed = false;
        self.notice = "pasted into the selected Zed cell editor".to_owned();
        Ok(NotebookInput::Consumed)
    }

    pub(crate) fn scroll(&mut self, down: bool, amount: usize) {
        self.scroll = if down {
            self.scroll.saturating_add(amount)
        } else {
            self.scroll.saturating_sub(amount)
        };
        self.delete_armed = false;
    }

    pub(crate) fn close(self, cx: &mut gpui::AsyncApp) -> Result<()> {
        cx.update_window(self.window.into(), |_root, window, _cx| {
            window.remove_window();
        })
        .context("close hidden Zed NotebookEditor window")
    }

    fn handle_command_key(
        &mut self,
        event: KeyEvent,
        body_height: usize,
        cx: &mut gpui::AsyncApp,
    ) -> Result<()> {
        match (event.code, event.modifiers) {
            (KeyCode::Up, KeyModifiers::NONE) => {
                self.dispatch_action(Box::new(MoveUp), cx)?;
                self.selected_cell = self.selected_cell.saturating_sub(1);
                self.notice = "selected previous cell".to_owned();
            }
            (KeyCode::Down, KeyModifiers::NONE) => {
                self.dispatch_action(Box::new(MoveDown), cx)?;
                self.selected_cell = self
                    .selected_cell
                    .saturating_add(1)
                    .min(self.cell_count.saturating_sub(1));
                self.notice = "selected next cell".to_owned();
            }
            (KeyCode::Enter, KeyModifiers::NONE) | (KeyCode::Char('e'), KeyModifiers::NONE) => {
                self.dispatch_action(Box::new(EnterEditMode), cx)?;
                self.mode = NotebookMode::Edit;
                self.notice = "editing selected cell with Zed Editor".to_owned();
            }
            (KeyCode::Char('b'), KeyModifiers::NONE) => {
                self.dispatch_action(Box::new(AddCodeBlock), cx)?;
                self.selected_cell = self.selected_cell.saturating_add(1);
                self.cell_count = self.cell_count.saturating_add(1);
                self.mode = NotebookMode::Edit;
                self.notice = "added a code cell below; edit mode".to_owned();
            }
            (KeyCode::Char('m'), KeyModifiers::NONE) => {
                self.dispatch_action(Box::new(AddMarkdownBlock), cx)?;
                self.selected_cell = self.selected_cell.saturating_add(1);
                self.cell_count = self.cell_count.saturating_add(1);
                self.mode = NotebookMode::Edit;
                self.notice = "added a Markdown cell below; edit mode".to_owned();
            }
            (KeyCode::Char('d'), KeyModifiers::NONE) if self.delete_armed => {
                self.dispatch_action(Box::new(DeleteCell), cx)?;
                self.cell_count = self.cell_count.saturating_sub(1);
                self.selected_cell = self.selected_cell.min(self.cell_count.saturating_sub(1));
                self.delete_armed = false;
                self.notice = "deleted selected cell".to_owned();
            }
            (KeyCode::Char('d'), KeyModifiers::NONE) => {
                self.delete_armed = true;
                self.notice = "press d again to delete the selected cell".to_owned();
                return Ok(());
            }
            (KeyCode::Up, KeyModifiers::ALT) => {
                self.dispatch_action(Box::new(MoveCellUp), cx)?;
                self.selected_cell = self.selected_cell.saturating_sub(1);
                self.notice = "moved selected cell up".to_owned();
            }
            (KeyCode::Down, KeyModifiers::ALT) => {
                self.dispatch_action(Box::new(MoveCellDown), cx)?;
                self.selected_cell = self
                    .selected_cell
                    .saturating_add(1)
                    .min(self.cell_count.saturating_sub(1));
                self.notice = "moved selected cell down".to_owned();
            }
            (KeyCode::Char('c'), KeyModifiers::NONE) => {
                self.preserved_outputs.clear();
                self.dispatch_action(Box::new(ClearOutputs), cx)?;
                self.notice = "cleared all notebook outputs".to_owned();
            }
            (KeyCode::Char('i'), KeyModifiers::NONE) => {
                self.dispatch_action(Box::new(InterruptKernel), cx)?;
                self.notice = "interrupt requested from the Zed Jupyter session".to_owned();
            }
            (KeyCode::Char('r'), KeyModifiers::NONE) => {
                let stale_pids = self.kernel_processes.matching_pids();
                let restart = self.dispatch_action(Box::new(RestartKernel), cx);
                let recovered = self.kernel_processes.terminate_pids(Some(&stale_pids));
                restart?;
                self.notice = if recovered == 0 {
                    "kernel restart requested".to_owned()
                } else {
                    format!(
                        "kernel restart requested; recovered {recovered} stale starting process(es)"
                    )
                };
            }
            (KeyCode::Char('R'), KeyModifiers::SHIFT)
            | (KeyCode::Char('r'), KeyModifiers::SHIFT) => {
                self.preserved_outputs.clear();
                self.dispatch_action(Box::new(RunAll), cx)?;
                self.notice = "all code cells submitted to the Zed Jupyter session".to_owned();
            }
            (KeyCode::PageUp, KeyModifiers::NONE) => {
                self.scroll = self.scroll.saturating_sub(body_height.max(1));
                self.notice = "notebook page up".to_owned();
            }
            (KeyCode::PageDown, KeyModifiers::NONE) => {
                self.scroll = self.scroll.saturating_add(body_height.max(1));
                self.notice = "notebook page down".to_owned();
            }
            _ => {
                self.notice = "Notebook keys: ↑/↓ select · Enter edit · Ctrl/Shift-Enter run · b/m add · dd delete · Alt-↑/↓ move · c clear · i interrupt · r restart · R run all".to_owned();
            }
        }
        if !matches!(
            (event.code, event.modifiers),
            (KeyCode::Char('d'), KeyModifiers::NONE)
        ) {
            self.delete_armed = false;
        }
        Ok(())
    }

    fn forget_selected_outputs(&mut self, cx: &mut gpui::AsyncApp) -> Result<()> {
        let value = notebook_value(&self.window, cx)?;
        if let Some(id) = notebook_cells(&value)
            .get(self.selected_cell)
            .and_then(|cell| cell.get("id"))
            .and_then(Value::as_str)
        {
            self.preserved_outputs.remove(id);
        }
        Ok(())
    }

    fn dispatch_action(&self, action: Box<dyn Action>, cx: &mut gpui::AsyncApp) -> Result<()> {
        cx.update_window(self.window.into(), move |_root, window, cx| {
            window.draw(cx).clear(cx);
            window.activate_window();
            window.dispatch_action(action, cx);
        })
        .context("dispatch action to Zed NotebookEditor")
    }

    fn dispatch_text(&self, text: String, cx: &mut gpui::AsyncApp) -> Result<()> {
        cx.update_window(self.window.into(), move |_root, window, cx| {
            window.draw(cx).clear(cx);
            window.activate_window();
            window.dispatch_action(Box::new(HandleInput(text)), cx);
        })
        .context("dispatch text input to Zed notebook cell editor")
    }

    fn dispatch_keystroke(&self, keystroke: Keystroke, cx: &mut gpui::AsyncApp) -> Result<()> {
        cx.update_window(self.window.into(), move |_root, window, cx| {
            window.draw(cx).clear(cx);
            window.activate_window();
            window.dispatch_keystroke(keystroke, cx);
        })
        .context("dispatch keystroke to Zed notebook cell editor")?;
        Ok(())
    }
}

fn open_notebook_window(
    project: Entity<Project>,
    item: Entity<NotebookItem>,
    cx: &mut App,
) -> Result<WindowHandle<NotebookEditor>> {
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(gpui::Bounds {
                origin: Default::default(),
                size: gpui::size(gpui::px(900.0), gpui::px(700.0)),
            })),
            focus: false,
            show: false,
            ..Default::default()
        },
        |window, cx| {
            let notebook = cx.new(|cx| NotebookEditor::new(project, item, window, cx));
            window.focus(&notebook.focus_handle(cx), cx);
            notebook
        },
    )
    .context("GPUI could not create the hidden Zed NotebookEditor window")
}

fn notebook_value(window: &WindowHandle<NotebookEditor>, cx: &mut gpui::AsyncApp) -> Result<Value> {
    window
        .update(cx, |notebook, _window, cx| {
            serde_json::to_value(notebook.to_notebook(cx))
        })
        .context("read Zed NotebookEditor")?
        .context("serialize Zed NotebookEditor snapshot")
}

fn notebook_cells(value: &Value) -> &[Value] {
    value
        .get("cells")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn collect_preserved_outputs(
    raw_notebook: &Value,
    authority: &Value,
) -> BTreeMap<String, Vec<Value>> {
    let raw_cells = notebook_cells(raw_notebook);
    notebook_cells(authority)
        .iter()
        .enumerate()
        .filter_map(|(index, authority_cell)| {
            let id = authority_cell.get("id")?.as_str()?;
            let outputs = raw_cells.get(index)?.get("outputs")?.as_array()?.clone();
            (!outputs.is_empty()).then(|| (id.to_owned(), outputs))
        })
        .collect()
}

fn merge_preserved_outputs(
    authority: &mut Value,
    preserved_outputs: &BTreeMap<String, Vec<Value>>,
) {
    let Some(cells) = authority.get_mut("cells").and_then(Value::as_array_mut) else {
        return;
    };
    for cell in cells {
        let Some(id) = cell.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(outputs) = preserved_outputs.get(id) else {
            continue;
        };
        if let Some(cell) = cell.as_object_mut() {
            cell.insert("outputs".to_owned(), Value::Array(outputs.clone()));
        }
    }
}

fn snapshot_from_value(
    notebook: &Value,
    selected_cell: usize,
    mode: NotebookMode,
    requested_scroll: usize,
    _width: usize,
    height: usize,
    notice: &str,
) -> NotebookSnapshot {
    let cells = notebook_cells(notebook);
    let selected_cell = selected_cell.min(cells.len().saturating_sub(1));
    let kernel = notebook
        .pointer("/metadata/kernelspec/display_name")
        .or_else(|| notebook.pointer("/metadata/kernelspec/name"))
        .and_then(Value::as_str)
        .unwrap_or("auto-detect");
    let mut rows = vec![NotebookRow {
        text: format!(
            " Notebook · {kernel} · {} cell{} · {} mode ",
            cells.len(),
            if cells.len() == 1 { "" } else { "s" },
            mode.label()
        ),
        selected: false,
        kind: NotebookRowKind::Title,
    }];
    let mut selected_range = 0..1;
    for (index, cell) in cells.iter().take(MAX_NOTEBOOK_CELLS).enumerate() {
        let start = rows.len();
        let selected = index == selected_cell;
        let kind = cell
            .get("cell_type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let execution = cell
            .get("execution_count")
            .filter(|value| !value.is_null())
            .map(value_to_text)
            .unwrap_or_else(|| " ".to_owned());
        rows.push(NotebookRow {
            text: format!(
                "{} Cell {} · {} · In[{execution}]",
                if selected { "▶" } else { " " },
                index + 1,
                kind
            ),
            selected,
            kind: NotebookRowKind::CellHeader,
        });
        let source = value_to_text(cell.get("source").unwrap_or(&Value::Null));
        let mut source_count = 0usize;
        for line in source.lines().take(MAX_SOURCE_LINES_PER_CELL) {
            source_count += 1;
            rows.push(NotebookRow {
                text: format!("  │ {}", bounded_line(line)),
                selected,
                kind: NotebookRowKind::Source,
            });
        }
        if source_count == 0 {
            rows.push(NotebookRow {
                text: "  │ ".to_owned(),
                selected,
                kind: NotebookRowKind::Source,
            });
        }
        let mut output_lines = 0usize;
        'outputs: for output in cell
            .get("outputs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            for (text, kind) in projected_output(output) {
                if output_lines >= MAX_OUTPUT_LINES_PER_CELL {
                    rows.push(NotebookRow {
                        text: "  ╰─ … output projection limit reached".to_owned(),
                        selected,
                        kind: NotebookRowKind::Hint,
                    });
                    break 'outputs;
                }
                output_lines += 1;
                rows.push(NotebookRow {
                    text: format!("  ╰─ {}", bounded_line(&text)),
                    selected,
                    kind,
                });
            }
        }
        rows.push(NotebookRow {
            text: String::new(),
            selected,
            kind: NotebookRowKind::Hint,
        });
        if selected {
            selected_range = start..rows.len();
        }
    }
    if cells.len() > MAX_NOTEBOOK_CELLS {
        rows.push(NotebookRow {
            text: format!(
                "… {} cells omitted by the {}-cell projection limit",
                cells.len() - MAX_NOTEBOOK_CELLS,
                MAX_NOTEBOOK_CELLS
            ),
            selected: false,
            kind: NotebookRowKind::Hint,
        });
    }

    let body_height = height.saturating_sub(1).max(1);
    let max_scroll = rows.len().saturating_sub(body_height);
    let mut scroll = requested_scroll.min(max_scroll);
    if selected_range.start < scroll {
        scroll = selected_range.start;
    } else if selected_range.start >= scroll.saturating_add(body_height) {
        scroll = selected_range
            .start
            .saturating_sub(body_height.saturating_sub(1));
    }
    scroll = scroll.min(max_scroll);

    NotebookSnapshot {
        rows,
        scroll,
        status: format!(
            "{notice}  |  Ctrl-S save · Ctrl-W close · ↑/↓ cell · Enter edit · Ctrl-Enter run"
        ),
    }
}

fn projected_output(output: &Value) -> Vec<(String, NotebookRowKind)> {
    let output_type = output
        .get("output_type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match output_type {
        "stream" => split_output_lines(value_to_text(output.get("text").unwrap_or(&Value::Null)))
            .into_iter()
            .map(|line| (line, NotebookRowKind::Output))
            .collect(),
        "error" => {
            let name = output
                .get("ename")
                .and_then(Value::as_str)
                .unwrap_or("Kernel Error");
            let value = output
                .get("evalue")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut lines = vec![(format!("{name}: {value}"), NotebookRowKind::Error)];
            lines.extend(
                split_output_lines(value_to_text(
                    output.get("traceback").unwrap_or(&Value::Null),
                ))
                .into_iter()
                .map(|line| (line, NotebookRowKind::Error)),
            );
            lines
        }
        "display_data" | "execute_result" => {
            let Some(data) = output.get("data").and_then(Value::as_object) else {
                return vec![("empty rich output".to_owned(), NotebookRowKind::Hint)];
            };
            for mime in [
                "text/plain",
                "text/markdown",
                "application/json",
                "text/html",
            ] {
                if let Some(value) = data.get(mime) {
                    return split_output_lines(value_to_text(value))
                        .into_iter()
                        .map(|line| (format!("{mime}: {line}"), NotebookRowKind::Output))
                        .collect();
                }
            }
            if let Some((mime, value)) = data.iter().find(|(mime, _)| mime.starts_with("image/")) {
                let encoded = value_to_text(value).len();
                return vec![(
                    format!(
                        "[{mime} output · {encoded} encoded bytes · terminal metadata fallback]"
                    ),
                    NotebookRowKind::Hint,
                )];
            }
            vec![(
                format!(
                    "rich output types: {}",
                    data.keys().cloned().collect::<Vec<_>>().join(", ")
                ),
                NotebookRowKind::Hint,
            )]
        }
        other => vec![(format!("[{other} output]"), NotebookRowKind::Hint)],
    }
}

fn split_output_lines(text: String) -> Vec<String> {
    let mut lines = text.lines().map(ToOwned::to_owned).collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn value_to_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(values) if values.iter().all(Value::is_string) => {
            values.iter().filter_map(Value::as_str).collect::<String>()
        }
        value => serde_json::to_string(value).unwrap_or_else(|_| "<invalid output>".to_owned()),
    }
}

fn bounded_line(line: &str) -> String {
    if line.len() <= MAX_PROJECTED_LINE_BYTES {
        return line.to_owned();
    }
    let mut end = MAX_PROJECTED_LINE_BYTES.saturating_sub('…'.len_utf8());
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}

fn is_run(event: &KeyEvent) -> bool {
    event.code == KeyCode::Enter && event.modifiers == KeyModifiers::CONTROL
}

fn is_run_and_advance(event: &KeyEvent) -> bool {
    event.code == KeyCode::Enter && event.modifiers == KeyModifiers::SHIFT
}

fn is_workspace_shortcut(event: &KeyEvent) -> bool {
    input::is_quit(event)
        || input::is_save(event)
        || input::is_reload(event)
        || input::is_close_tab(event)
        || input::is_previous_tab(event)
        || input::is_next_tab(event)
        || input::is_new_tab(event)
        || input::is_open(event)
        || input::is_quick_open(event)
        || input::is_project_search(event)
        || input::is_toggle_project_panel(event)
        || input::is_toggle_git_panel(event)
        || input::is_toggle_outline_panel(event)
        || input::is_toggle_terminal_panel(event)
        || input::is_new_terminal(event)
        || input::is_run_task(event)
        || input::is_rerun_task(event)
        || input::is_toggle_debugger_panel(event)
        || input::is_start_debugging(event)
        || input::is_debug_repl(event)
        || input::is_command_palette(event)
        || input::is_extensions(event)
        || input::is_select_theme(event)
        || input::is_select_icon_theme(event)
        || input::is_open_settings(event)
        || input::is_open_keymap(event)
        || input::is_reload_extensions(event)
        || input::is_check_updates(event)
        || input::is_terminal_capabilities(event)
        || input::is_toggle_agent_panel(event)
        || input::is_toggle_collaboration_panel(event)
        || input::is_workspace_layout_shortcut(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_only_the_exact_zed_kernel_connection_argument() {
        let expected = OsStr::new("kernel-zed-42.json");
        assert!(is_connection_file_argument(
            OsStr::new("/tmp/jupyter/kernel-zed-42.json"),
            expected
        ));
        assert!(!is_connection_file_argument(
            OsStr::new("/tmp/jupyter/kernel-zed-43.json"),
            expected
        ));
        assert!(!is_connection_file_argument(
            OsStr::new("--connection=/tmp/jupyter/kernel-zed-42.json"),
            expected
        ));
    }

    #[test]
    fn projects_code_markdown_stream_error_and_image_fallback() {
        let notebook = json!({
            "metadata": {"kernelspec": {"display_name": "Fixture Python"}},
            "nbformat": 4,
            "nbformat_minor": 5,
            "cells": [
                {
                    "cell_type": "markdown",
                    "id": "m",
                    "metadata": {},
                    "source": ["# Heading\n", "日本語"]
                },
                {
                    "cell_type": "code",
                    "id": "c",
                    "metadata": {},
                    "execution_count": 7,
                    "source": ["print('ok')\n"],
                    "outputs": [
                        {"output_type": "stream", "name": "stdout", "text": ["ok\n"]},
                        {"output_type": "error", "ename": "ValueError", "evalue": "bad", "traceback": ["trace"]},
                        {"output_type": "display_data", "metadata": {}, "data": {"image/png": "AAAA"}}
                    ]
                }
            ]
        });
        let snapshot = snapshot_from_value(&notebook, 1, NotebookMode::Command, 0, 80, 30, "ready");
        let text = snapshot
            .rows
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Notebook · Fixture Python · 2 cells · command mode"));
        assert!(text.contains("# Heading"));
        assert!(text.contains("日本語"));
        assert!(text.contains("In[7]"));
        assert!(text.contains("ok"));
        assert!(text.contains("ValueError: bad"));
        assert!(text.contains("image/png output · 4 encoded bytes"));
        assert!(
            snapshot
                .rows
                .iter()
                .any(|row| row.selected && row.text.contains("Cell 2"))
        );
    }

    #[test]
    fn preserves_outputs_that_upstream_notebook_serialization_omits() {
        let raw = json!({
            "cells": [{
                "id": "cell-a",
                "outputs": [{
                    "data": {"image/png": "aGVsbG8="},
                    "metadata": {},
                    "output_type": "display_data"
                }]
            }]
        });
        let mut authority = json!({
            "cells": [{"cell_type": "code", "id": "cell-a", "outputs": []}]
        });
        let preserved = collect_preserved_outputs(&raw, &authority);
        merge_preserved_outputs(&mut authority, &preserved);

        assert_eq!(
            authority.pointer("/cells/0/outputs/0/output_type"),
            Some(&json!("display_data"))
        );
        assert_eq!(
            authority.pointer("/cells/0/outputs/0/data/image~1png"),
            Some(&json!("aGVsbG8="))
        );
    }

    #[test]
    fn output_projection_limit_is_global_per_cell() {
        let outputs = (0..MAX_OUTPUT_LINES_PER_CELL + 100)
            .map(|_| json!({"output_type": "stream", "name": "stdout", "text": "x"}))
            .collect::<Vec<_>>();
        let notebook = json!({
            "metadata": {},
            "nbformat": 4,
            "nbformat_minor": 5,
            "cells": [{
                "cell_type": "code",
                "id": "bounded-output",
                "metadata": {},
                "execution_count": null,
                "source": [],
                "outputs": outputs
            }]
        });

        let snapshot = snapshot_from_value(&notebook, 0, NotebookMode::Command, 0, 80, 20, "ready");
        assert_eq!(
            snapshot
                .rows
                .iter()
                .filter(|row| row.kind == NotebookRowKind::Output)
                .count(),
            MAX_OUTPUT_LINES_PER_CELL
        );
        assert_eq!(
            snapshot
                .rows
                .iter()
                .filter(|row| row.text.contains("output projection limit reached"))
                .count(),
            1
        );
    }

    #[test]
    fn selected_cell_is_scrolled_into_the_bounded_view() {
        let cells = (0..100)
            .map(|index| {
                json!({
                    "cell_type": "code",
                    "id": format!("cell-{index}"),
                    "metadata": {},
                    "execution_count": null,
                    "source": [format!("CELL_{index}")],
                    "outputs": []
                })
            })
            .collect::<Vec<_>>();
        let notebook = json!({"metadata": {}, "nbformat": 4, "nbformat_minor": 5, "cells": cells});
        let snapshot = snapshot_from_value(&notebook, 99, NotebookMode::Edit, 0, 40, 8, "editing");
        let visible = snapshot
            .rows
            .iter()
            .skip(snapshot.scroll)
            .take(7)
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(visible.contains("Cell 100"));
        assert!(snapshot.status.contains("Ctrl-S save"));
    }
}
