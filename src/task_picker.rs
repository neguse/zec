//! Bounded picker projection for tasks resolved by Zed's TaskInventory.

use project::TaskSourceKind;
use task::ResolvedTask;
use unicode_width::UnicodeWidthStr;

use crate::{
    prompt::LinePrompt,
    render::{OverlayRow, OverlaySnapshot},
    tabs::Direction,
};

#[derive(Clone, Debug)]
pub(crate) struct TaskCandidate {
    pub(crate) source: TaskSourceKind,
    pub(crate) task: ResolvedTask,
}

impl TaskCandidate {
    fn searchable_text(&self) -> String {
        format!(
            "{} {} {}",
            self.task.display_label(),
            self.task.resolved.command_label,
            source_label(&self.source)
        )
        .to_lowercase()
    }

    fn row(&self) -> String {
        let command = self.task.resolved.command_label.trim();
        if command.is_empty() || command == self.task.display_label() {
            format!(
                "{}  · {}",
                self.task.display_label(),
                source_label(&self.source)
            )
        } else {
            format!(
                "{}  · {}  · {}",
                self.task.display_label(),
                source_label(&self.source),
                command
            )
        }
    }
}

#[derive(Debug)]
pub(crate) struct TaskPickerPrompt {
    pub(crate) prompt: LinePrompt,
    candidates: Vec<TaskCandidate>,
    visible: Vec<usize>,
    selected: usize,
    feedback: Option<String>,
}

impl TaskPickerPrompt {
    pub(crate) fn new(candidates: Vec<TaskCandidate>) -> Self {
        let mut this = Self {
            prompt: LinePrompt::new(),
            candidates,
            visible: Vec::new(),
            selected: 0,
            feedback: None,
        };
        this.refresh();
        this
    }

    pub(crate) fn refresh(&mut self) {
        let query = self.prompt.text().trim().to_lowercase();
        self.visible = self
            .candidates
            .iter()
            .enumerate()
            .filter_map(|(index, candidate)| {
                (query.is_empty() || fuzzy_match(&candidate.searchable_text(), &query))
                    .then_some(index)
            })
            .collect();
        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
        self.feedback = None;
    }

    pub(crate) fn step(&mut self, direction: Direction) {
        if self.visible.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = match direction {
            Direction::Previous => self.selected.saturating_sub(1),
            Direction::Next => (self.selected + 1).min(self.visible.len() - 1),
        };
    }

    pub(crate) fn selected_candidate(&self) -> Option<TaskCandidate> {
        self.visible
            .get(self.selected)
            .and_then(|index| self.candidates.get(*index))
            .cloned()
    }

    pub(crate) fn status(&self, message: Option<&str>) -> (String, usize) {
        let message_prefix = message.map_or_else(String::new, |message| format!("{message}  |  "));
        let prefix = format!("{message_prefix}Task: ");
        let cursor = prefix.width().saturating_add(
            self.prompt
                .text()
                .get(..self.prompt.cursor())
                .unwrap_or_default()
                .width(),
        );
        let suffix = self
            .feedback
            .as_deref()
            .map(|message| format!("  |  {message}"))
            .unwrap_or_default();
        let status = format!(
            "{prefix}{}  ({}/{})  Enter run  ↑/↓ select  Esc cancel{suffix}",
            self.prompt.text(),
            if self.visible.is_empty() {
                0
            } else {
                self.selected + 1
            },
            self.visible.len()
        );
        (status, cursor)
    }

    pub(crate) fn overlay(&self) -> OverlaySnapshot {
        const LIMIT: usize = 12;
        let start = self
            .selected
            .saturating_sub(LIMIT / 2)
            .min(self.visible.len().saturating_sub(LIMIT));
        let end = start.saturating_add(LIMIT).min(self.visible.len());
        OverlaySnapshot {
            title: "Tasks".to_owned(),
            rows: self.visible[start..end]
                .iter()
                .filter_map(|index| self.candidates.get(*index))
                .map(|candidate| OverlayRow {
                    text: candidate.row(),
                    enabled: true,
                })
                .collect(),
            selected: (!self.visible.is_empty()).then_some(self.selected.saturating_sub(start)),
        }
    }

    pub(crate) fn set_feedback(&mut self, feedback: impl Into<String>) {
        self.feedback = Some(feedback.into());
    }
}

fn source_label(source: &TaskSourceKind) -> String {
    match source {
        TaskSourceKind::UserInput => "one-shot".to_owned(),
        TaskSourceKind::AbsPath { abs_path, .. } => abs_path.display().to_string(),
        TaskSourceKind::Worktree {
            directory_in_worktree,
            ..
        } => directory_in_worktree.as_unix_str().to_owned(),
        TaskSourceKind::Language { name } => format!("language:{name}"),
        TaskSourceKind::Lsp {
            language_name,
            server,
        } => format!("lsp:{language_name}:{server:?}"),
    }
}

fn fuzzy_match(candidate: &str, query: &str) -> bool {
    let mut characters = candidate.chars();
    query
        .chars()
        .all(|needle| characters.by_ref().any(|candidate| candidate == needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_matching_is_ordered_and_case_normalized_by_the_caller() {
        assert!(fuzzy_match("cargo test workspace", "ctw"));
        assert!(fuzzy_match("cargo test workspace", "test"));
        assert!(!fuzzy_match("cargo test workspace", "wct"));
    }

    #[test]
    fn task_source_labels_are_stable() {
        assert_eq!(source_label(&TaskSourceKind::UserInput), "one-shot");
        assert_eq!(
            source_label(&TaskSourceKind::Language {
                name: "Rust".into()
            }),
            "language:Rust"
        );
    }
}
