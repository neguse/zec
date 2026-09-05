//! The one event type every change to the application passes through.

use std::path::PathBuf;

use crossterm::event::{KeyEvent, MouseEvent};

use super::feature::FeatureEvent;
use crate::{
    terminal::{self, ResizeAcknowledgement, ScrollDirection},
    zed,
};

#[derive(Debug)]
pub enum Event {
    Input(Input),
    /// Held until the frame after the resize has been drawn.
    Resize(ResizeAcknowledgement),
    /// Something visible changed; the next frame reads the new snapshot.
    Redraw,
    FocusChanged(bool),
    Signal(i32),
    /// The terminal reader stopped; the session cannot continue.
    Fatal(String),
    Document(DocumentEvent),
    Config(ConfigEvent),
    /// Zed refused to start repository-controlled processes in the
    /// worktree until it is trusted.
    WorktreeRestricted(PathBuf),
    /// A language server started or stopped.
    LanguageServer {
        name: String,
        running: bool,
    },
    /// A completion of work a feature spawned.
    Feature(FeatureEvent),
}

#[derive(Debug)]
pub enum Input {
    Key(KeyEvent),
    Paste(String),
    Mouse(MouseEvent),
    Scroll(ScrollDirection),
}

#[derive(Debug)]
pub enum DocumentEvent {
    ReloadFinished {
        buffer_id: u64,
        result: Result<(), String>,
    },
}

#[derive(Debug)]
pub struct ConfigEvent {
    pub kind: &'static str,
    pub result: Result<String, String>,
}

impl From<terminal::Event> for Event {
    fn from(event: terminal::Event) -> Self {
        match event {
            terminal::Event::Key(key) => Self::Input(Input::Key(key)),
            terminal::Event::Paste(text) => Self::Input(Input::Paste(text)),
            terminal::Event::Mouse(mouse) => Self::Input(Input::Mouse(mouse)),
            terminal::Event::Scroll(direction) => Self::Input(Input::Scroll(direction)),
            terminal::Event::FocusChanged(focused) => Self::FocusChanged(focused),
            terminal::Event::Resize(acknowledgement) => Self::Resize(acknowledgement),
            terminal::Event::Signal(signal) => Self::Signal(signal),
            terminal::Event::Error(error) => Self::Fatal(format!("terminal input failed: {error}")),
        }
    }
}

impl From<zed::Event> for Event {
    fn from(event: zed::Event) -> Self {
        match event {
            zed::Event::Redraw => Self::Redraw,
            zed::Event::ReloadFinished { buffer_id, result } => {
                Self::Document(DocumentEvent::ReloadFinished { buffer_id, result })
            }
            zed::Event::ConfigReloaded { kind, result } => {
                Self::Config(ConfigEvent { kind, result })
            }
            zed::Event::WorktreeRestricted { path } => Self::WorktreeRestricted(path),
            zed::Event::LanguageServer { name, running } => Self::LanguageServer { name, running },
        }
    }
}
