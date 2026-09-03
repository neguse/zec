//! The application: one state tree, one event type, one update, one draw.

pub mod command;
pub mod documents;
mod draw;
pub mod event;
pub mod feature;
pub mod overlay;
pub mod status;
pub mod tabs;
mod update;
pub mod workspace;

use std::{
    cell::RefCell,
    env,
    io::{self, IsTerminal as _},
    path::PathBuf,
    rc::Rc,
    sync::mpsc,
};

use anyhow::{Context as _, Result, bail};
use async_channel::Sender;
use gpui::AsyncApp;

use crate::{
    cli,
    terminal::{self, Capabilities, InputReader, ResizeAcknowledgement, Session},
    zed::{
        self,
        keymap::Lookup,
        runtime::{self, Runtime},
        services::Services,
    },
};
use command::Command;
use documents::{Document, Documents};
use draw::Frame;
use event::Event;
use feature::Features;
use overlay::Overlays;
use status::Status;
use workspace::WorkspaceModel;

/// zec's own bindings, applied after Zed's defaults and before the user's
/// `keymap.json`.
const DEFAULT_KEYMAP: &str = include_str!("keymap.json");

/// The key context in which prompts and pickers resolve keys.
pub const OVERLAY_CONTEXT: &str = "zec_overlay";

/// What the loop does after an update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Flow {
    Continue,
    Suspend,
    Exit,
}

pub struct App {
    services: Services,
    events: Sender<Event>,
    pending_commands: Rc<RefCell<Vec<Command>>>,
    keymap: Rc<RefCell<Lookup>>,
    capabilities: Capabilities,
    cwd: PathBuf,
    /// The visible worktree root, when zec opened a directory.
    root: Option<PathBuf>,
    workspace: WorkspaceModel,
    documents: Documents,
    overlays: Overlays,
    status: Status,
    features: Features,
    /// The last drawn frame, for mouse hit testing and scrolling.
    frame: Option<Frame>,
    /// Released after the first frame following a resize has been drawn.
    resize: Option<ResizeAcknowledgement>,
    needs_invalidate: bool,
}

struct Startup {
    paths: Vec<PathBuf>,
    cwd: PathBuf,
    services: Services,
    events: Sender<Event>,
    pending_commands: Rc<RefCell<Vec<Command>>>,
    keymap: Rc<RefCell<Lookup>>,
    capabilities: Capabilities,
}

impl App {
    async fn start(startup: Startup, cx: &mut AsyncApp) -> Result<Self> {
        let Startup {
            paths,
            cwd,
            services,
            events,
            pending_commands,
            keymap,
            capabilities,
        } = startup;

        let mut errors = Vec::new();
        let mut root = None;
        let mut documents = Documents::default();
        let mut items = Vec::new();

        let directory = match paths.as_slice() {
            [] => Some(cwd.clone()),
            [path] => services
                .metadata(path)
                .await?
                .filter(|metadata| metadata.is_dir)
                .map(|_| path.clone()),
            _ => None,
        };
        if let Some(directory) = directory {
            services.add_root(&directory, cx).await?;
            root = Some(directory);
        } else {
            for path in &paths {
                match services.open_file(path, cx).await {
                    Ok(buffer) => {
                        if documents.item_for_buffer(&buffer).is_some() {
                            continue;
                        }
                        let document = Document::open(buffer, None, &services, &events, cx)?;
                        items.push(documents.insert(document));
                    }
                    Err(error) => {
                        errors.push(format!("failed to open {}: {error:#}", path.display()))
                    }
                }
            }
        }
        if items.is_empty() {
            let buffer = cx.update(|cx| services.create_scratch(cx));
            let label = documents.next_untitled_label();
            let document = Document::open(buffer, Some(label), &services, &events, cx)?;
            items.push(documents.insert(document));
        }

        let mut workspace = WorkspaceModel::new(items[0]);
        for item in &items[1..] {
            workspace.open_item(*item).context("register startup tab")?;
        }
        workspace
            .focus_item(items[0])
            .context("focus first startup tab")?;

        let mut status = Status::default();
        if !errors.is_empty() {
            status.set(errors.join("; "));
        }

        let app = Self {
            services,
            events,
            pending_commands,
            keymap,
            capabilities,
            cwd,
            root,
            workspace,
            documents,
            overlays: Overlays::default(),
            status,
            features: Features::default(),
            frame: None,
            resize: None,
            needs_invalidate: false,
        };
        app.check_invariants()?;
        Ok(app)
    }

    /// Every update ends with the tree checked: the workspace reducer's own
    /// invariants, and one document per item.
    fn check_invariants(&self) -> Result<()> {
        self.workspace.validate().context("workspace invariants")?;
        let items = self.workspace.item_ids();
        for item in &items {
            if self.documents.get(*item).is_none() {
                bail!("workspace item {item:?} has no document");
            }
        }
        if items.len() != self.documents.len() {
            bail!(
                "{} workspace items but {} documents",
                items.len(),
                self.documents.len()
            );
        }
        Ok(())
    }
}

/// Runs the interactive editor until it exits.
pub fn run(paths: Vec<PathBuf>) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("zec requires a terminal; use --smoke for the headless check");
    }
    let cwd = env::current_dir().context("could not determine the current directory")?;
    let paths = cli::absolute_unique_paths(&cwd, paths)?;

    // Action and configuration events stay lossless within a hard bound;
    // redundant redraws use try_send and may be dropped.
    let (events, receiver) = async_channel::bounded::<Event>(64);
    let session = Session::enter()?;
    // The reader starts only after raw mode is active: an earlier start
    // races the first frame and can swallow the first keystroke.
    let mut reader = InputReader::spawn(events.clone())?;
    let capabilities = session.capabilities().clone();
    let mut terminal = session.terminal()?;
    let (result_sender, result_receiver) = mpsc::sync_channel::<Result<()>>(1);

    runtime::application().run(move |cx| {
        runtime::init(cx);
        let pending_commands = Rc::new(RefCell::new(Vec::new()));
        Command::intercept(pending_commands.clone(), cx);
        let keymap = match zed::keymap::apply(DEFAULT_KEYMAP, Vec::new(), cx) {
            Ok(lookup) => Rc::new(RefCell::new(lookup)),
            Err(error) => {
                let _ = result_sender.send(Err(error));
                cx.quit();
                return;
            }
        };
        let fs = cx.global::<Runtime>().fs.clone();
        zed::config::start_watchers(
            fs,
            DEFAULT_KEYMAP,
            events.clone(),
            {
                let keymap = keymap.clone();
                move |lookup| *keymap.borrow_mut() = lookup
            },
            cx,
        );
        let services = Services::local(cx);
        let startup = Startup {
            paths,
            cwd,
            services,
            events,
            pending_commands,
            keymap,
            capabilities,
        };

        cx.spawn(async move |cx| {
            let result = async {
                let mut app = App::start(startup, cx).await?;
                app.draw_frame(&mut terminal, cx)?;
                loop {
                    let event = receiver.recv().await.context("terminal input stopped")?;
                    match app.update(event, cx).await? {
                        Flow::Continue => {}
                        Flow::Suspend => {
                            terminal::suspend_and_resume(&mut terminal, &app.capabilities)
                                .context("suspend and resume the terminal")?;
                            app.status.set("resumed");
                        }
                        Flow::Exit => break,
                    }
                    app.draw_frame(&mut terminal, cx)?;
                }
                app.shutdown(cx);
                Ok(())
            }
            .await;
            let _ = result_sender.send(result);
            let _ = cx.update(|cx| cx.quit());
        })
        .detach();
    });

    // Cleanup order: join the reader, restore the terminal, then drop the
    // reader to remove the signal handlers.
    reader.stop_and_join();
    session.restore().context("restore the terminal")?;
    let result = result_receiver.try_recv().unwrap_or(Ok(()));
    drop(reader);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_binding_resolves_to_a_registered_command() {
        let keymap = settings::KeymapFile::parse(DEFAULT_KEYMAP).expect("default keymap parses");
        let mut zec_bindings = 0;
        for section in keymap.sections() {
            for (keystroke, action) in section.bindings() {
                let action = action.to_string();
                let Some(name) = action.strip_prefix("zec::") else {
                    continue;
                };
                assert!(
                    Command::from_action_name(&action).is_some(),
                    "{keystroke} is bound to unknown command zec::{name}"
                );
                zec_bindings += 1;
            }
        }
        assert!(zec_bindings > 0, "the default keymap binds no zec command");
    }
}
