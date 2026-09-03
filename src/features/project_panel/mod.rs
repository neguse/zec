//! Project panel: the visible worktree as a tree in the left dock.
//!
//! The rows are read from Zed's worktree snapshot on every frame, walking
//! only the directories that are expanded, so the panel holds no copy of
//! the tree: just which directories are open, which entry is selected,
//! and whether ignored entries are shown. Mutations go through the
//! `Project`, never the filesystem, so the worktree learns about them at
//! once; a worktree change redraws the frame.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use gpui::AsyncApp;
use project::{Entry, ProjectEntryId, Worktree};

use crate::{
    app::{
        command::Command,
        event::Event,
        feature::{Ctx, FeatureEvent, PanelOutcome},
        overlay::{Overlay, PromptTarget},
    },
    terminal::{
        prompt::LinePrompt,
        render::{OverlayRow, OverlaySnapshot, window_start},
    },
    zed::services,
};

/// The key context of this panel's own bindings.
pub const KEY_CONTEXT: &str = "zec_project_panel";

#[derive(Debug)]
pub enum ProjectPanelEvent {
    MutationFinished {
        description: String,
        /// The entry to reveal on success.
        reveal: Option<PathBuf>,
        result: Result<(), String>,
    },
}

/// One visible row of the tree.
struct Row {
    id: ProjectEntryId,
    path: PathBuf,
    name: String,
    is_dir: bool,
    depth: usize,
    ignored: bool,
    expanded: bool,
}

#[derive(Default)]
pub struct ProjectPanel {
    expanded: BTreeSet<ProjectEntryId>,
    selected: Option<ProjectEntryId>,
    show_ignored: bool,
    /// The first row the last frame showed, for mouse hit testing.
    first_row: usize,
    /// Redraws on every worktree change while the panel has been shown.
    watch: Option<gpui::Subscription>,
}

impl ProjectPanel {
    /// The dock was shown: watch the worktree and reveal the active file.
    pub fn shown(&mut self, ctx: &mut Ctx, cx: &mut AsyncApp) {
        if self.watch.is_none()
            && let Some(worktree) = ctx.services.visible_worktree(cx)
        {
            self.watch = Some(services::watch_worktree(&worktree, ctx.events.clone(), cx));
        }
        self.reveal_active(ctx, cx);
    }

    /// Runs one panel command while this panel has focus.
    pub fn execute(
        &mut self,
        ctx: &mut Ctx,
        command: Command,
        confirmed: bool,
        cx: &mut AsyncApp,
    ) -> PanelOutcome {
        let rows = self.rows(ctx, cx);
        let index = self.select_within(&rows);
        match command {
            Command::PanelSelectNext => {
                if let Some(row) = index.and_then(|index| rows.get(index + 1)) {
                    self.selected = Some(row.id);
                }
            }
            Command::PanelSelectPrevious => {
                if let Some(row) = index.and_then(|index| rows.get(index.checked_sub(1)?)) {
                    self.selected = Some(row.id);
                }
            }
            Command::PanelExpand => {
                if let Some(row) = index.and_then(|index| rows.get(index)) {
                    if row.is_dir && !row.expanded {
                        self.expanded.insert(row.id);
                    } else if let Some(next) = rows.get(index.unwrap_or(0) + 1) {
                        self.selected = Some(next.id);
                    }
                }
            }
            Command::PanelCollapse => {
                if let Some(row) = index.and_then(|index| rows.get(index)) {
                    if row.expanded {
                        self.expanded.remove(&row.id);
                    } else if let Some(parent) = parent_index(&rows, index.unwrap_or(0)) {
                        self.selected = Some(rows[parent].id);
                    }
                }
            }
            Command::PanelActivate => {
                if let Some(row) = index.and_then(|index| rows.get(index)) {
                    if row.is_dir {
                        if !self.expanded.remove(&row.id) {
                            self.expanded.insert(row.id);
                        }
                    } else {
                        return PanelOutcome::Open(row.path.clone());
                    }
                }
            }
            Command::PanelNewFile | Command::PanelNewDirectory => {
                let Some(directory) = self.directory_for_new_entry(ctx, &rows, index) else {
                    ctx.status.set("no directory selected");
                    return PanelOutcome::Consumed;
                };
                let (label, target) = if command == Command::PanelNewFile {
                    ("New file", PromptTarget::PanelNewFile { directory })
                } else {
                    (
                        "New directory",
                        PromptTarget::PanelNewDirectory { directory },
                    )
                };
                ctx.overlays.clear();
                ctx.overlays.push(Overlay::Prompt {
                    label,
                    line: LinePrompt::new(),
                    target,
                    feedback: None,
                });
            }
            Command::PanelRename => {
                let Some(row) = index.and_then(|index| rows.get(index)) else {
                    ctx.status.set("nothing selected");
                    return PanelOutcome::Consumed;
                };
                let Some(directory) = row.path.parent().map(Path::to_path_buf) else {
                    return PanelOutcome::Consumed;
                };
                ctx.overlays.clear();
                ctx.overlays.push(Overlay::Prompt {
                    label: "Rename",
                    line: LinePrompt::with_text(&row.name),
                    target: PromptTarget::PanelRename {
                        entry: row.id,
                        directory,
                    },
                    feedback: None,
                });
            }
            Command::PanelDelete => {
                let Some(row) = index.and_then(|index| rows.get(index)) else {
                    ctx.status.set("nothing selected");
                    return PanelOutcome::Consumed;
                };
                if !confirmed {
                    ctx.overlays.push(Overlay::Confirm {
                        message: format!(
                            "delete {}? press {} again to confirm",
                            row.name,
                            ctx.key_hint(Command::PanelDelete)
                        ),
                        command: Command::PanelDelete,
                    });
                    return PanelOutcome::Consumed;
                }
                self.mutate(
                    ctx,
                    format!("deleted {}", row.name),
                    None,
                    Mutation::Delete(row.id),
                    cx,
                );
            }
            Command::PanelRevealActive => {
                if !self.reveal_active(ctx, cx) {
                    ctx.status.set("the active tab has no file in the project");
                }
            }
            Command::PanelToggleIgnored => {
                self.show_ignored = !self.show_ignored;
                ctx.status.set(if self.show_ignored {
                    "showing ignored entries"
                } else {
                    "hiding ignored entries"
                });
            }
            _ => {}
        }
        PanelOutcome::Consumed
    }

    /// A prompt of this panel was submitted.
    pub fn submit(&mut self, ctx: &mut Ctx, target: &PromptTarget, text: &str, cx: &mut AsyncApp) {
        let name = text.trim();
        if name.is_empty() || Path::new(name).is_absolute() {
            ctx.overlays
                .set_feedback("enter a name relative to the directory");
            return;
        }
        let (description, reveal, mutation) = match target {
            PromptTarget::PanelNewFile { directory } => {
                let path = directory.join(name);
                (
                    format!("created {}", relative(ctx, &path)),
                    Some(path.clone()),
                    Mutation::Create {
                        path,
                        is_directory: false,
                    },
                )
            }
            PromptTarget::PanelNewDirectory { directory } => {
                let path = directory.join(name);
                (
                    format!("created {}", relative(ctx, &path)),
                    Some(path.clone()),
                    Mutation::Create {
                        path,
                        is_directory: true,
                    },
                )
            }
            PromptTarget::PanelRename { entry, directory } => {
                let path = directory.join(name);
                (
                    format!("renamed to {}", relative(ctx, &path)),
                    Some(path.clone()),
                    Mutation::Rename {
                        entry: *entry,
                        path,
                    },
                )
            }
            _ => return,
        };
        ctx.overlays.pop();
        self.mutate(ctx, description, reveal, mutation, cx);
    }

    pub fn update(&mut self, ctx: &mut Ctx, event: ProjectPanelEvent, cx: &mut AsyncApp) {
        let ProjectPanelEvent::MutationFinished {
            description,
            reveal,
            result,
        } = event;
        match result {
            Ok(()) => {
                if let Some(path) = reveal {
                    self.reveal_path(ctx, &path, cx);
                }
                ctx.status.set(description);
            }
            Err(error) => ctx.status.set(format!("{description} failed: {error}")),
        }
    }

    /// The rows to draw, windowed so the selection stays visible.
    pub fn view(&mut self, ctx: &mut Ctx, row_budget: usize, cx: &mut AsyncApp) -> OverlaySnapshot {
        let rows = self.rows(ctx, cx);
        let selected = self.select_within(&rows);
        self.first_row = window_start(rows.len(), selected, row_budget);
        let mut title = ctx
            .root
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Project".to_owned());
        if self.show_ignored {
            title.push_str(" +ignored");
        }
        OverlaySnapshot {
            title: format!(" {title} "),
            rows: rows
                .iter()
                .map(|row| OverlayRow {
                    text: format!(
                        "{}{}{}{}",
                        "  ".repeat(row.depth),
                        match (row.is_dir, row.expanded) {
                            (true, true) => "▾ ",
                            (true, false) => "▸ ",
                            (false, _) => "  ",
                        },
                        row.name,
                        if row.ignored { " [ignored]" } else { "" }
                    ),
                    enabled: !row.ignored,
                })
                .collect(),
            selected,
        }
    }

    /// A click on the panel body selects the row under the pointer.
    pub fn click(&mut self, ctx: &mut Ctx, screen_row: usize, cx: &mut AsyncApp) {
        let rows = self.rows(ctx, cx);
        if let Some(row) = rows.get(self.first_row + screen_row) {
            self.selected = Some(row.id);
        }
    }

    /// The selected row's index; a selection that is not among the rows
    /// (none yet, or the entry went away) falls back to the first row.
    fn select_within(&mut self, rows: &[Row]) -> Option<usize> {
        let index = self
            .selected
            .and_then(|selected| rows.iter().position(|row| row.id == selected))
            .or_else(|| (!rows.is_empty()).then_some(0))?;
        self.selected = Some(rows[index].id);
        Some(index)
    }

    /// Where a new entry goes: the selected directory, or the directory of
    /// the selected file, or the root.
    fn directory_for_new_entry(
        &self,
        ctx: &Ctx,
        rows: &[Row],
        index: Option<usize>,
    ) -> Option<PathBuf> {
        match index.and_then(|index| rows.get(index)) {
            Some(row) if row.is_dir => Some(row.path.clone()),
            Some(row) => row.path.parent().map(Path::to_path_buf),
            None => ctx.root.map(Path::to_path_buf),
        }
    }

    /// Selects the active document's file, opening its ancestors.
    fn reveal_active(&mut self, ctx: &mut Ctx, cx: &mut AsyncApp) -> bool {
        let Some(path) = ctx
            .documents
            .get(ctx.workspace.active_item())
            .and_then(|document| document.state(cx).path)
        else {
            return false;
        };
        self.reveal_path(ctx, &path, cx)
    }

    fn reveal_path(&mut self, ctx: &mut Ctx, path: &Path, cx: &mut AsyncApp) -> bool {
        let Some(worktree) = ctx.services.visible_worktree(cx) else {
            return false;
        };
        let found = worktree.read_with(cx, |worktree, _| {
            let entry = worktree
                .entries(true, 0)
                .find(|entry| worktree.absolutize(&entry.path) == path)?;
            let ancestors = entry
                .path
                .ancestors()
                .skip(1)
                .filter_map(|ancestor| worktree.entry_for_path(ancestor))
                .map(|ancestor| ancestor.id)
                .collect::<Vec<_>>();
            Some((entry.id, entry.is_ignored, ancestors))
        });
        let Some((id, ignored, ancestors)) = found else {
            return false;
        };
        self.expanded.extend(ancestors);
        if ignored {
            self.show_ignored = true;
        }
        self.selected = Some(id);
        true
    }

    /// The visible rows: the children of the root and of every expanded
    /// directory, depth first, directories before files, names compared
    /// case-insensitively.
    fn rows(&self, ctx: &Ctx, cx: &AsyncApp) -> Vec<Row> {
        let Some(worktree) = ctx.services.visible_worktree(cx) else {
            return Vec::new();
        };
        worktree.read_with(cx, |worktree, _| {
            let mut rows = Vec::new();
            if let Some(root) = worktree.root_entry() {
                self.push_children(worktree, root, 0, &mut rows);
            }
            rows
        })
    }

    fn push_children(
        &self,
        worktree: &Worktree,
        parent: &Entry,
        depth: usize,
        rows: &mut Vec<Row>,
    ) {
        let mut children = worktree
            .child_entries(&parent.path)
            .filter(|entry| self.show_ignored || !entry.is_ignored)
            .collect::<Vec<_>>();
        children.sort_by(|left, right| {
            right.is_dir().cmp(&left.is_dir()).then_with(|| {
                name_of(left)
                    .to_lowercase()
                    .cmp(&name_of(right).to_lowercase())
            })
        });
        for entry in children {
            let expanded = entry.is_dir() && self.expanded.contains(&entry.id);
            rows.push(Row {
                id: entry.id,
                path: worktree.absolutize(&entry.path),
                name: name_of(entry).to_owned(),
                is_dir: entry.is_dir(),
                depth,
                ignored: entry.is_ignored,
                expanded,
            });
            if expanded {
                self.push_children(worktree, entry, depth + 1, rows);
            }
        }
    }

    fn mutate(
        &mut self,
        ctx: &mut Ctx,
        description: String,
        reveal: Option<PathBuf>,
        mutation: Mutation,
        cx: &mut AsyncApp,
    ) {
        let services = ctx.services.clone();
        let events = ctx.events.clone();
        cx.spawn(async move |cx| {
            let result = match mutation {
                Mutation::Create { path, is_directory } => {
                    services.create_entry(&path, is_directory, cx).await
                }
                Mutation::Rename { entry, path } => services.rename_entry(entry, &path, cx).await,
                Mutation::Delete(entry) => services.delete_entry(entry, cx).await,
            };
            let _ = events
                .send(Event::Feature(FeatureEvent::ProjectPanel(
                    ProjectPanelEvent::MutationFinished {
                        description,
                        reveal,
                        result: result.map_err(|error| format!("{error:#}")),
                    },
                )))
                .await;
        })
        .detach();
    }
}

enum Mutation {
    Create {
        path: PathBuf,
        is_directory: bool,
    },
    Rename {
        entry: ProjectEntryId,
        path: PathBuf,
    },
    Delete(ProjectEntryId),
}

fn name_of(entry: &Entry) -> &str {
    entry.path.file_name().unwrap_or_default()
}

/// The nearest row above `index` one level up.
fn parent_index(rows: &[Row], index: usize) -> Option<usize> {
    let depth = rows.get(index)?.depth.checked_sub(1)?;
    (0..index)
        .rev()
        .find(|candidate| rows[*candidate].depth == depth)
}

fn relative(ctx: &Ctx, path: &Path) -> String {
    ctx.root
        .and_then(|root| path.strip_prefix(root).ok())
        .unwrap_or(path)
        .display()
        .to_string()
}
