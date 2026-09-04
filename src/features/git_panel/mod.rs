//! Git panel: the active repository's status in the left dock.
//!
//! Rows are Zed's repository snapshot read on every frame, sectioned into
//! conflicts, staged, unstaged, and untracked entries. Staging and commits
//! go through Zed's `Repository`; the panel holds no copy of the status,
//! only the selection, and a repository event redraws the frame.

use git::{
    repository::{AskPassDelegate, CommitOptions, RepoPath},
    status::{FileStatus, StageStatus, StatusCode},
};
use gpui::{AsyncApp, Subscription};
use project::git_store::{Repository, RepositorySnapshot, StatusEntry};

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
};

/// The key context of this panel's own bindings.
pub const KEY_CONTEXT: &str = "zec_git_panel";

#[derive(Debug)]
pub enum GitPanelEvent {
    Finished {
        description: String,
        result: Result<(), String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Section {
    Conflicts,
    Staged,
    Changes,
    Untracked,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Self::Conflicts => "Conflicts",
            Self::Staged => "Staged Changes",
            Self::Changes => "Changes",
            Self::Untracked => "Untracked Files",
        }
    }
}

#[derive(Clone)]
struct Entry {
    section: Section,
    path: RepoPath,
    /// The porcelain status letter of the relevant side of the index.
    code: char,
    added: u32,
    deleted: u32,
}

enum Row {
    Header(&'static str),
    Entry(Entry),
}

#[derive(Default)]
pub struct GitPanel {
    /// Section and unix path, so the selection survives a status refresh.
    selected: Option<(Section, String)>,
    /// The first row the last frame showed, for mouse hit testing.
    first_row: usize,
    /// Redraws on every repository change once the panel has been shown.
    watch: Option<Subscription>,
}

impl GitPanel {
    /// The dock was shown: watch the repository for changes. Zed may still
    /// be scanning, so the body reports a missing repository, not the
    /// status row.
    pub fn shown(&mut self, ctx: &mut Ctx, cx: &mut AsyncApp) {
        if self.watch.is_none() {
            self.watch = Some(ctx.services.watch_git_store(ctx.events.clone(), cx));
        }
    }

    /// Runs one panel command while this panel has focus.
    pub fn execute(&mut self, ctx: &mut Ctx, command: Command, cx: &mut AsyncApp) -> PanelOutcome {
        let Some(repository) = ctx.services.active_repository(cx) else {
            ctx.status.set("no git repository");
            return PanelOutcome::Consumed;
        };
        let snapshot = repository.read_with(cx, |repository, _| repository.snapshot());
        let rows = rows(&snapshot);
        let index = self.select_within(&rows);
        let selected = index.and_then(|index| match &rows[index] {
            Row::Entry(entry) => Some(entry.clone()),
            Row::Header(_) => None,
        });
        match command {
            Command::PanelSelectNext => {
                if let Some(next) = index.and_then(|index| next_entry(&rows, index, true)) {
                    self.select(&rows, next);
                }
            }
            Command::PanelSelectPrevious => {
                if let Some(previous) = index.and_then(|index| next_entry(&rows, index, false)) {
                    self.select(&rows, previous);
                }
            }
            Command::PanelActivate => {
                if let Some(entry) = selected {
                    return PanelOutcome::Open(snapshot.repo_path_to_abs_path(&entry.path));
                }
            }
            Command::GitToggleStaged => {
                let Some(entry) = selected else {
                    ctx.status.set("no entry selected");
                    return PanelOutcome::Consumed;
                };
                let stage = entry.section != Section::Staged;
                let description = format!(
                    "{} {}",
                    if stage { "staged" } else { "unstaged" },
                    entry.path.as_unix_str()
                );
                self.run(ctx, description, cx, move |repository, cx| {
                    if stage {
                        repository.stage_entries(vec![entry.path], cx)
                    } else {
                        repository.unstage_entries(vec![entry.path], cx)
                    }
                });
            }
            Command::GitStageAll => {
                self.run(
                    ctx,
                    "staged all changes".to_owned(),
                    cx,
                    |repository, cx| repository.stage_all(cx),
                );
            }
            Command::GitUnstageAll => {
                self.run(
                    ctx,
                    "unstaged all changes".to_owned(),
                    cx,
                    |repository, cx| repository.unstage_all(cx),
                );
            }
            Command::GitCommit => {
                if !rows
                    .iter()
                    .any(|row| matches!(row, Row::Entry(entry) if entry.section == Section::Staged))
                {
                    ctx.status.set("nothing staged to commit");
                    return PanelOutcome::Consumed;
                }
                ctx.overlays.clear();
                ctx.overlays.push(Overlay::Prompt {
                    label: "Commit message",
                    line: LinePrompt::new(),
                    target: PromptTarget::GitCommit,
                    feedback: None,
                });
            }
            _ => {}
        }
        PanelOutcome::Consumed
    }

    /// The commit prompt was submitted.
    pub fn submit_commit(&mut self, ctx: &mut Ctx, message: &str, cx: &mut AsyncApp) {
        let message = message.trim().to_owned();
        if message.is_empty() {
            ctx.overlays.set_feedback("enter a commit message");
            return;
        }
        let Some(repository) = ctx.services.active_repository(cx) else {
            ctx.overlays.set_feedback("no git repository");
            return;
        };
        ctx.overlays.pop();
        let events = ctx.events.clone();
        let description = format!("committed: {message}");
        cx.spawn(async move |cx| {
            // No password prompt reaches the terminal; a hook or signing
            // step that asks for one fails the commit instead.
            let askpass = AskPassDelegate::new(cx, |_prompt, _reply, _cx| {});
            let receiver = repository.update(cx, |repository, cx| {
                repository.commit(message.into(), None, CommitOptions::default(), askpass, cx)
            });
            let result = match receiver.await {
                Ok(result) => result.map_err(|error| format!("{error:#}")),
                Err(_) => Err("commit was cancelled".to_owned()),
            };
            let _ = events
                .send(Event::Feature(FeatureEvent::GitPanel(
                    GitPanelEvent::Finished {
                        description,
                        result,
                    },
                )))
                .await;
        })
        .detach();
    }

    pub fn update(&mut self, ctx: &mut Ctx, event: GitPanelEvent) {
        let GitPanelEvent::Finished {
            description,
            result,
        } = event;
        match result {
            Ok(()) => ctx.status.set(description),
            Err(error) => ctx.status.set(format!("{description} failed: {error}")),
        }
    }

    /// The rows to draw, windowed so the selection stays visible.
    pub fn view(&mut self, ctx: &mut Ctx, row_budget: usize, cx: &mut AsyncApp) -> OverlaySnapshot {
        let Some(repository) = ctx.services.active_repository(cx) else {
            self.first_row = 0;
            return OverlaySnapshot {
                title: " Git ".to_owned(),
                rows: vec![OverlayRow {
                    text: "no git repository".to_owned(),
                    enabled: false,
                }],
                selected: None,
            };
        };
        let snapshot = repository.read_with(cx, |repository, _| repository.snapshot());
        let rows = rows(&snapshot);
        let selected = self.select_within(&rows);
        self.first_row = window_start(rows.len(), selected, row_budget);
        let branch = snapshot
            .branch
            .as_ref()
            .map(|branch| branch.name().to_owned())
            .unwrap_or_else(|| "detached HEAD".to_owned());
        OverlaySnapshot {
            title: format!(" Git · {} · {branch} ", snapshot.display_name()),
            rows: rows
                .iter()
                .map(|row| match row {
                    Row::Header(title) => OverlayRow {
                        text: (*title).to_owned(),
                        enabled: false,
                    },
                    Row::Entry(entry) => OverlayRow {
                        text: format!(
                            "  {} {}  +{} -{}",
                            entry.code,
                            entry.path.as_unix_str(),
                            entry.added,
                            entry.deleted
                        ),
                        enabled: true,
                    },
                })
                .collect(),
            selected,
        }
    }

    /// A click on the panel body selects the entry under the pointer.
    pub fn click(&mut self, ctx: &mut Ctx, screen_row: usize, cx: &mut AsyncApp) {
        let Some(repository) = ctx.services.active_repository(cx) else {
            return;
        };
        let snapshot = repository.read_with(cx, |repository, _| repository.snapshot());
        let rows = rows(&snapshot);
        self.select(&rows, self.first_row + screen_row);
    }

    fn select(&mut self, rows: &[Row], index: usize) {
        if let Some(Row::Entry(entry)) = rows.get(index) {
            self.selected = Some((entry.section, entry.path.as_unix_str().to_owned()));
        }
    }

    /// The selected entry's row, falling back to the first entry when the
    /// selection is gone.
    fn select_within(&mut self, rows: &[Row]) -> Option<usize> {
        let index = self
            .selected
            .as_ref()
            .and_then(|(section, path)| {
                rows.iter().position(|row| {
                    matches!(row, Row::Entry(entry)
                        if entry.section == *section && entry.path.as_unix_str() == path)
                })
            })
            .or_else(|| rows.iter().position(|row| matches!(row, Row::Entry(_))))?;
        self.select(rows, index);
        Some(index)
    }

    fn run(
        &mut self,
        ctx: &mut Ctx,
        description: String,
        cx: &mut AsyncApp,
        operation: impl FnOnce(
            &mut Repository,
            &mut gpui::Context<Repository>,
        ) -> gpui::Task<anyhow::Result<()>>
        + 'static,
    ) {
        let Some(repository) = ctx.services.active_repository(cx) else {
            return;
        };
        let events = ctx.events.clone();
        cx.spawn(async move |cx| {
            let task = repository.update(cx, operation);
            let result = task.await.map_err(|error| format!("{error:#}"));
            let _ = events
                .send(Event::Feature(FeatureEvent::GitPanel(
                    GitPanelEvent::Finished {
                        description,
                        result,
                    },
                )))
                .await;
        })
        .detach();
    }
}

/// The status entries sectioned and sorted, each section under its header.
fn rows(snapshot: &RepositorySnapshot) -> Vec<Row> {
    let mut entries = snapshot
        .status()
        .filter(|entry| !entry.status.is_ignored())
        .flat_map(sectioned)
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        left.section
            .cmp(&right.section)
            .then_with(|| left.path.cmp(&right.path))
    });
    let mut rows = Vec::with_capacity(entries.len() + 4);
    let mut current = None;
    for entry in entries {
        if current != Some(entry.section) {
            current = Some(entry.section);
            rows.push(Row::Header(entry.section.title()));
        }
        rows.push(Row::Entry(entry));
    }
    rows
}

/// A partially staged file appears under both Staged and Changes.
fn sectioned(entry: StatusEntry) -> Vec<Entry> {
    if entry.status.is_conflicted() {
        return vec![sectioned_entry(&entry, Section::Conflicts, 'U', false)];
    }
    if entry.status.is_untracked() {
        return vec![sectioned_entry(&entry, Section::Untracked, '?', false)];
    }
    match entry.status.staging() {
        StageStatus::Staged => {
            let code = status_code(entry.status, true);
            vec![sectioned_entry(&entry, Section::Staged, code, true)]
        }
        StageStatus::Unstaged => {
            let code = status_code(entry.status, false);
            vec![sectioned_entry(&entry, Section::Changes, code, false)]
        }
        StageStatus::PartiallyStaged => vec![
            sectioned_entry(
                &entry,
                Section::Staged,
                status_code(entry.status, true),
                true,
            ),
            sectioned_entry(
                &entry,
                Section::Changes,
                status_code(entry.status, false),
                false,
            ),
        ],
    }
}

fn sectioned_entry(entry: &StatusEntry, section: Section, code: char, staged: bool) -> Entry {
    let stat = if staged {
        entry.staged_diff_stat.or(entry.diff_stat)
    } else {
        entry.unstaged_diff_stat.or(entry.diff_stat)
    };
    Entry {
        section,
        path: entry.repo_path.clone(),
        code,
        added: stat.map(|stat| stat.added).unwrap_or_default(),
        deleted: stat.map(|stat| stat.deleted).unwrap_or_default(),
    }
}

fn status_code(status: FileStatus, staged: bool) -> char {
    let FileStatus::Tracked(status) = status else {
        return if status.is_untracked() { '?' } else { 'U' };
    };
    let code = if staged {
        status.index_status
    } else {
        status.worktree_status
    };
    match code {
        StatusCode::Modified => 'M',
        StatusCode::TypeChanged => 'T',
        StatusCode::Added => 'A',
        StatusCode::Deleted => 'D',
        StatusCode::Renamed => 'R',
        StatusCode::Copied => 'C',
        StatusCode::Unmodified => '·',
    }
}

/// The nearest entry row after (or before) `index`.
fn next_entry(rows: &[Row], index: usize, forward: bool) -> Option<usize> {
    if forward {
        (index + 1..rows.len()).find(|candidate| matches!(rows[*candidate], Row::Entry(_)))
    } else {
        (0..index)
            .rev()
            .find(|candidate| matches!(rows[*candidate], Row::Entry(_)))
    }
}
