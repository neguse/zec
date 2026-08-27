//! Presentation-neutral projection of Zed's Git repository snapshot.

use git::status::{FileStatus, StageStatus, StatusCode};
use project::git_store::{RepositorySnapshot, StatusEntry};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Clear, Widget},
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum GitSection {
    Conflicts,
    Staged,
    Changes,
    Untracked,
}

impl GitSection {
    fn title(self) -> &'static str {
        match self {
            Self::Conflicts => "Conflicts",
            Self::Staged => "Staged Changes",
            Self::Changes => "Changes",
            Self::Untracked => "Untracked Files",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitEntry {
    pub(crate) section: GitSection,
    pub(crate) repo_path: git::repository::RepoPath,
    pub(crate) status: FileStatus,
    pub(crate) status_code: char,
    pub(crate) additions: u64,
    pub(crate) deletions: u64,
}

impl GitEntry {
    pub(crate) fn should_stage(&self) -> bool {
        self.section != GitSection::Staged
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitPanelSnapshot {
    pub(crate) repository_id: u64,
    pub(crate) scan_id: u64,
    pub(crate) repository_name: String,
    pub(crate) branch: String,
    pub(crate) ahead: u32,
    pub(crate) behind: u32,
    pub(crate) entries: Vec<GitEntry>,
}

impl GitPanelSnapshot {
    pub(crate) fn capture(repository: &RepositorySnapshot) -> Self {
        let mut entries = repository
            .status()
            .filter(|entry| !entry.status.is_ignored())
            .flat_map(project_status_entry)
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.section
                .cmp(&right.section)
                .then_with(|| left.repo_path.cmp(&right.repo_path))
        });
        let (ahead, behind) = repository
            .branch
            .as_ref()
            .and_then(|branch| branch.tracking_status())
            .map(|tracking| (tracking.ahead, tracking.behind))
            .unwrap_or_default();
        Self {
            repository_id: repository.id.to_proto(),
            scan_id: repository.scan_id,
            repository_name: repository.display_name().to_string(),
            branch: repository
                .branch
                .as_ref()
                .map(|branch| branch.name().to_owned())
                .unwrap_or_else(|| "detached HEAD".to_owned()),
            ahead,
            behind,
            entries,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitPanelState {
    snapshot: GitPanelSnapshot,
    selected: usize,
}

impl GitPanelState {
    pub(crate) fn new(snapshot: GitPanelSnapshot) -> Self {
        Self {
            snapshot,
            selected: 0,
        }
    }

    pub(crate) fn apply_snapshot(&mut self, snapshot: GitPanelSnapshot) {
        let selected = self
            .selected_entry()
            .map(|entry| (entry.section, entry.repo_path.as_unix_str().to_owned()));
        self.snapshot = snapshot;
        self.selected = selected
            .and_then(|(section, path)| {
                self.snapshot.entries.iter().position(|entry| {
                    entry.section == section && entry.repo_path.as_unix_str() == path
                })
            })
            .unwrap_or_else(|| {
                self.selected
                    .min(self.snapshot.entries.len().saturating_sub(1))
            });
    }

    pub(crate) fn repository_id(&self) -> u64 {
        self.snapshot.repository_id
    }

    pub(crate) fn scan_id(&self) -> u64 {
        self.snapshot.scan_id
    }

    pub(crate) fn move_selection(&mut self, delta: isize) {
        if self.snapshot.entries.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.snapshot.entries.len() - 1);
    }

    pub(crate) fn selected_entry(&self) -> Option<&GitEntry> {
        self.snapshot.entries.get(self.selected)
    }

    pub(crate) fn has_unstaged(&self) -> bool {
        self.snapshot
            .entries
            .iter()
            .any(|entry| entry.section != GitSection::Staged)
    }

    fn visible_window(&self, row_budget: usize) -> (usize, usize) {
        if self.snapshot.entries.is_empty() || row_budget == 0 {
            return (0, 0);
        }
        let budget = row_budget.min(self.snapshot.entries.len());
        let start = self
            .selected
            .saturating_sub(budget / 2)
            .min(self.snapshot.entries.len() - budget);
        (start, start + budget)
    }

    fn title(&self) -> String {
        let tracking = match (self.snapshot.ahead, self.snapshot.behind) {
            (0, 0) => String::new(),
            (ahead, 0) => format!(" ↑{ahead}"),
            (0, behind) => format!(" ↓{behind}"),
            (ahead, behind) => format!(" ↑{ahead} ↓{behind}"),
        };
        format!(
            " Git · {} · {}{} · {} ",
            self.snapshot.repository_name,
            self.snapshot.branch,
            tracking,
            self.snapshot.entries.len()
        )
    }
}

pub(crate) struct GitPanelWidget<'a> {
    state: &'a GitPanelState,
    focused: bool,
}

impl<'a> GitPanelWidget<'a> {
    pub(crate) fn new(state: &'a GitPanelState, focused: bool) -> Self {
        Self { state, focused }
    }
}

impl Widget for GitPanelWidget<'_> {
    fn render(self, area: Rect, buffer: &mut Buffer) {
        if area.width < 3 || area.height < 3 {
            return;
        }
        Clear.render(area, buffer);
        let border_style = if self.focused {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.state.title())
            .border_style(border_style);
        let inner = block.inner(area);
        block.render(area, buffer);
        if inner.is_empty() {
            return;
        }
        if self.state.snapshot.entries.is_empty() {
            buffer.set_string(
                inner.x,
                inner.y,
                "✓ Working tree clean",
                Style::default().fg(Color::Green),
            );
            return;
        }

        // Reserve section headers while choosing the entry window, then render
        // a bounded slice. A one-row dock still always exposes the selection.
        let entry_budget = usize::from(inner.height).saturating_sub(1).max(1);
        let (start, end) = self.state.visible_window(entry_budget);
        let mut y = inner.y;
        let mut previous_section = None;
        for (index, entry) in self.state.snapshot.entries[start..end].iter().enumerate() {
            if previous_section != Some(entry.section) && y < inner.bottom() {
                buffer.set_string(
                    inner.x,
                    y,
                    entry.section.title(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                );
                y = y.saturating_add(1);
                previous_section = Some(entry.section);
            }
            if y >= inner.bottom() {
                break;
            }
            let absolute_index = start + index;
            let selected = absolute_index == self.state.selected;
            let prefix = if selected { "›" } else { " " };
            let stats = if entry.additions == 0 && entry.deletions == 0 {
                String::new()
            } else {
                format!("  +{} -{}", entry.additions, entry.deletions)
            };
            let label = format!(
                "{prefix} {}  {}{stats}",
                entry.status_code,
                entry.repo_path.as_unix_str()
            );
            let mut style = section_style(entry.section);
            if selected {
                style = style.add_modifier(Modifier::REVERSED);
            }
            buffer.set_style(Rect::new(inner.x, y, inner.width, 1), style);
            buffer.set_stringn(inner.x, y, label, usize::from(inner.width), style);
            y = y.saturating_add(1);
        }
    }
}

fn project_status_entry(entry: StatusEntry) -> Vec<GitEntry> {
    if entry.status.is_conflicted() {
        return vec![git_entry(entry, GitSection::Conflicts, 'U', false)];
    }
    if entry.status.is_untracked() {
        return vec![git_entry(entry, GitSection::Untracked, '?', false)];
    }
    match entry.status.staging() {
        StageStatus::Staged => {
            let code = status_code(entry.status, true);
            vec![git_entry(entry, GitSection::Staged, code, true)]
        }
        StageStatus::Unstaged => {
            let code = status_code(entry.status, false);
            vec![git_entry(entry, GitSection::Changes, code, false)]
        }
        StageStatus::PartiallyStaged => {
            let staged_code = status_code(entry.status, true);
            let unstaged_code = status_code(entry.status, false);
            vec![
                git_entry(entry.clone(), GitSection::Staged, staged_code, true),
                git_entry(entry, GitSection::Changes, unstaged_code, false),
            ]
        }
    }
}

fn git_entry(entry: StatusEntry, section: GitSection, status_code: char, staged: bool) -> GitEntry {
    let stat = if staged {
        entry.staged_diff_stat.or(entry.diff_stat)
    } else {
        entry.unstaged_diff_stat.or(entry.diff_stat)
    };
    GitEntry {
        section,
        repo_path: entry.repo_path,
        status: entry.status,
        status_code,
        additions: stat.map(|stat| u64::from(stat.added)).unwrap_or_default(),
        deletions: stat.map(|stat| u64::from(stat.deleted)).unwrap_or_default(),
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

fn section_style(section: GitSection) -> Style {
    Style::default().fg(match section {
        GitSection::Conflicts => Color::LightRed,
        GitSection::Staged => Color::LightGreen,
        GitSection::Changes => Color::LightYellow,
        GitSection::Untracked => Color::DarkGray,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::{repository::RepoPath, status::TrackedStatus};
    use project::git_store::StatusEntry;

    fn entry(path: &str, status: FileStatus) -> StatusEntry {
        StatusEntry {
            repo_path: RepoPath::from_proto(path).unwrap(),
            status,
            diff_stat: None,
            staged_diff_stat: None,
            unstaged_diff_stat: None,
        }
    }

    #[test]
    fn partially_staged_files_are_projected_into_both_sections() {
        let projected = project_status_entry(entry(
            "src/main.rs",
            FileStatus::Tracked(TrackedStatus {
                index_status: StatusCode::Added,
                worktree_status: StatusCode::Modified,
            }),
        ));
        assert_eq!(projected.len(), 2);
        assert_eq!(projected[0].section, GitSection::Staged);
        assert_eq!(projected[0].status_code, 'A');
        assert_eq!(projected[1].section, GitSection::Changes);
        assert_eq!(projected[1].status_code, 'M');
        assert!(!projected[0].should_stage());
        assert!(projected[1].should_stage());
    }

    #[test]
    fn conflicts_and_untracked_files_have_distinct_actions() {
        let conflict = project_status_entry(entry(
            "conflict.rs",
            FileStatus::Unmerged(git::status::UnmergedStatus {
                first_head: git::status::UnmergedStatusCode::Updated,
                second_head: git::status::UnmergedStatusCode::Updated,
            }),
        ));
        let untracked = project_status_entry(entry("new.rs", FileStatus::Untracked));
        assert_eq!(conflict[0].section, GitSection::Conflicts);
        assert_eq!(conflict[0].status_code, 'U');
        assert_eq!(untracked[0].section, GitSection::Untracked);
        assert_eq!(untracked[0].status_code, '?');
    }

    #[test]
    fn selection_is_clamped_and_visible_window_tracks_it() {
        let entries = (0..20)
            .map(|index| {
                project_status_entry(entry(
                    &format!("file-{index:02}.rs"),
                    FileStatus::worktree(StatusCode::Modified),
                ))
                .remove(0)
            })
            .collect();
        let mut state = GitPanelState::new(GitPanelSnapshot {
            repository_id: 1,
            scan_id: 1,
            repository_name: "repo".to_owned(),
            branch: "main".to_owned(),
            ahead: 0,
            behind: 0,
            entries,
        });
        state.move_selection(100);
        assert_eq!(state.selected, 19);
        let (start, end) = state.visible_window(5);
        assert_eq!((start, end), (15, 20));
    }
}
