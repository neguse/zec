//! The feature contract and the composition of every feature.
//!
//! A feature is a leaf: it owns its state and events, and reaches the rest
//! of zec only through [`Ctx`]. This file and the dispatch arms in
//! `update.rs` are the only places that name a feature.

use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
};

use async_channel::Sender;
use editor::Editor;
use gpui::WindowHandle;

use super::{
    command::Command, documents::Documents, event::Event, overlay::Overlays, status::Status,
    workspace::WorkspaceModel,
};
use crate::{
    features::{
        buffer_search::{BufferSearch, BufferSearchEvent},
        git_panel::{GitPanel, GitPanelEvent},
        language::{Language, LanguageEvent},
        outline_panel::OutlinePanel,
        project_panel::{ProjectPanel, ProjectPanelEvent},
        project_search::{ProjectSearch, ProjectSearchEvent},
        quick_open::{QuickOpen, QuickOpenEvent},
        sessions::Sessions,
        tasks::{Tasks, TasksEvent},
        terminal_panel::{TerminalPanel, TerminalPanelEvent},
        theme_picker::ThemePicker,
    },
    terminal::keys,
    zed::{keymap::Lookup, services::Services},
};

/// What a feature may touch while it runs.
pub struct Ctx<'a> {
    pub services: &'a Services,
    /// The visible worktree root, when zec opened a directory.
    pub root: Option<&'a Path>,
    /// The active document's hidden window: Zed's Editor APIs, without
    /// the rest of `Documents`.
    pub editor: WindowHandle<Editor>,
    /// Read views of the pane tree and its documents.
    pub workspace: &'a WorkspaceModel,
    pub documents: &'a Documents,
    pub overlays: &'a mut Overlays,
    pub status: &'a mut Status,
    /// For completions of work a feature spawned.
    pub events: &'a Sender<Event>,
    /// The live keymap, for naming keys in messages.
    pub keymap: &'a Rc<RefCell<Lookup>>,
}

impl Ctx<'_> {
    /// The key currently bound to `command`, as the status row shows it.
    pub fn key_hint(&self, command: Command) -> String {
        key_hint(&self.keymap.borrow(), command)
    }
}

/// The key bound to `command`, or its label in backticks when unbound.
pub fn key_hint(keymap: &Lookup, command: Command) -> String {
    keymap
        .keystroke_for(command.action_name())
        .map(|keystroke| keys::display_keystroke(&keystroke))
        .unwrap_or_else(|| format!("`{}`", command.label()))
}

/// What a panel command asks the app to do afterwards.
pub enum PanelOutcome {
    Consumed,
    /// Open a file in the active pane.
    Open(PathBuf),
    /// Move the caret of the active document and return focus to it.
    Jump {
        point: text::Point,
        label: String,
    },
}

/// One field per feature.
#[derive(Default)]
pub struct Features {
    pub quick_open: QuickOpen,
    pub project_search: ProjectSearch,
    pub buffer_search: BufferSearch,
    pub sessions: Sessions,
    pub project_panel: ProjectPanel,
    pub outline_panel: OutlinePanel,
    pub language: Language,
    pub git_panel: GitPanel,
    pub terminal_panel: TerminalPanel,
    pub tasks: Tasks,
    pub theme_picker: ThemePicker,
}

/// One variant per feature, wrapping that feature's own event.
#[derive(Debug)]
pub enum FeatureEvent {
    QuickOpen(QuickOpenEvent),
    ProjectSearch(ProjectSearchEvent),
    BufferSearch(BufferSearchEvent),
    ProjectPanel(ProjectPanelEvent),
    Language(LanguageEvent),
    GitPanel(GitPanelEvent),
    TerminalPanel(TerminalPanelEvent),
    Tasks(TasksEvent),
}
