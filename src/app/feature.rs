//! The feature contract and the composition of every feature.
//!
//! A feature is a leaf: it owns its state and events, and reaches the rest
//! of zec only through [`Ctx`]. This file and the dispatch arms in
//! `update.rs` are the only places that name a feature.

use std::path::Path;

use async_channel::Sender;
use editor::Editor;
use gpui::WindowHandle;

use super::{event::Event, overlay::Overlays, status::Status};
use crate::{
    features::{
        buffer_search::{BufferSearch, BufferSearchEvent},
        project_search::{ProjectSearch, ProjectSearchEvent},
        quick_open::{QuickOpen, QuickOpenEvent},
    },
    zed::services::Services,
};

/// What a feature may touch while it runs.
pub struct Ctx<'a> {
    pub services: &'a Services,
    /// The visible worktree root, when zec opened a directory.
    pub root: Option<&'a Path>,
    /// The active document's hidden window: Zed's Editor APIs, without
    /// the rest of `Documents`.
    pub editor: WindowHandle<Editor>,
    pub overlays: &'a mut Overlays,
    pub status: &'a mut Status,
    /// For completions of work a feature spawned.
    pub events: &'a Sender<Event>,
}

/// One field per feature.
#[derive(Default)]
pub struct Features {
    pub quick_open: QuickOpen,
    pub project_search: ProjectSearch,
    pub buffer_search: BufferSearch,
}

/// One variant per feature, wrapping that feature's own event.
#[derive(Debug)]
pub enum FeatureEvent {
    QuickOpen(QuickOpenEvent),
    ProjectSearch(ProjectSearchEvent),
    BufferSearch(BufferSearchEvent),
}
