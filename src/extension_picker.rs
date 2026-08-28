//! Bounded terminal projection for Zed's extension store.

use std::{cmp::Ordering, sync::Arc};

use semver::Version;
use unicode_width::UnicodeWidthStr;

use crate::{
    prompt::{LinePrompt, PromptAction},
    render::{OverlayRow, OverlaySnapshot},
    tabs::Direction,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtensionOperationStatus {
    Install,
    Upgrade,
    Remove,
}

impl ExtensionOperationStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Install => "installing",
            Self::Upgrade => "updating",
            Self::Remove => "removing",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExtensionRecord {
    pub(crate) id: Arc<str>,
    pub(crate) name: String,
    pub(crate) latest_version: Arc<str>,
    pub(crate) installed_version: Option<Arc<str>>,
    pub(crate) description: Option<String>,
    pub(crate) download_count: u64,
    pub(crate) dev: bool,
    pub(crate) operation: Option<ExtensionOperationStatus>,
    pub(crate) provides: Vec<String>,
}

impl ExtensionRecord {
    pub(crate) fn is_installed(&self) -> bool {
        self.installed_version.is_some()
    }

    pub(crate) fn has_update(&self) -> bool {
        let Some(installed) = self.installed_version.as_deref() else {
            return false;
        };
        if self.dev {
            return false;
        }
        match (
            Version::parse(installed),
            Version::parse(&self.latest_version),
        ) {
            (Ok(installed), Ok(latest)) => latest > installed,
            _ => installed != self.latest_version.as_ref(),
        }
    }

    fn searchable_text(&self) -> String {
        format!(
            "{} {} {} {} {}",
            self.id,
            self.name,
            self.description.as_deref().unwrap_or_default(),
            self.latest_version,
            self.provides.join(" ")
        )
        .to_lowercase()
    }

    fn row(&self) -> String {
        let state = if let Some(operation) = self.operation {
            operation.label().to_owned()
        } else if self.dev {
            format!(
                "dev {}",
                self.installed_version.as_deref().unwrap_or("unknown")
            )
        } else if self.has_update() {
            format!(
                "update {}→{}",
                self.installed_version.as_deref().unwrap_or("?"),
                self.latest_version
            )
        } else if let Some(version) = self.installed_version.as_deref() {
            format!("installed {version}")
        } else {
            format!("available {}", self.latest_version)
        };
        let provides = if self.provides.is_empty() {
            String::new()
        } else {
            format!("  · {}", self.provides.join(","))
        };
        format!("{}  · {}  · {state}{provides}", self.name, self.id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtensionScope {
    All,
    Installed,
    Updates,
}

impl ExtensionScope {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Installed,
            Self::Installed => Self::Updates,
            Self::Updates => Self::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Installed => "installed",
            Self::Updates => "updates",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ExtensionPickerCommand {
    Install { id: Arc<str> },
    InstallDev { source: String },
    Upgrade { id: Arc<str>, version: Arc<str> },
    RebuildDev { id: Arc<str> },
    Uninstall { id: Arc<str> },
    ReloadAll,
}

#[derive(Debug)]
pub(crate) struct ExtensionPickerPrompt {
    pub(crate) prompt: LinePrompt,
    records: Vec<ExtensionRecord>,
    visible: Vec<usize>,
    selected: usize,
    scope: ExtensionScope,
    loading: bool,
    feedback: Option<String>,
    dev_path: Option<LinePrompt>,
}

impl ExtensionPickerPrompt {
    pub(crate) fn new(records: Vec<ExtensionRecord>) -> Self {
        let mut this = Self {
            prompt: LinePrompt::new(),
            records,
            visible: Vec::new(),
            selected: 0,
            scope: ExtensionScope::All,
            loading: false,
            feedback: None,
            dev_path: None,
        };
        this.sort_records();
        this.refresh();
        this
    }

    #[cfg(test)]
    pub(crate) fn records(&self) -> &[ExtensionRecord] {
        &self.records
    }

    pub(crate) fn set_loading(&mut self, loading: bool) {
        self.loading = loading;
    }

    pub(crate) fn set_feedback(&mut self, feedback: impl Into<String>) {
        self.feedback = Some(feedback.into());
    }

    pub(crate) fn begin_dev_install(&mut self) {
        self.dev_path = Some(LinePrompt::new());
        self.feedback = None;
    }

    pub(crate) fn is_entering_dev_path(&self) -> bool {
        self.dev_path.is_some()
    }

    pub(crate) fn handle_dev_key(&mut self, key: &crossterm::event::KeyEvent) -> PromptAction {
        let Some(prompt) = self.dev_path.as_mut() else {
            return PromptAction::Ignored;
        };
        match prompt.handle_key(key) {
            PromptAction::Cancel => {
                self.dev_path = None;
                self.feedback = Some("dev extension install cancelled".to_owned());
                PromptAction::Cancel
            }
            action => action,
        }
    }

    pub(crate) fn handle_dev_paste(&mut self, text: &str) -> PromptAction {
        self.dev_path
            .as_mut()
            .map_or(PromptAction::Ignored, |prompt| prompt.handle_paste(text))
    }

    pub(crate) fn dev_install_command(&mut self) -> Option<ExtensionPickerCommand> {
        let source = self.dev_path.as_ref()?.text().trim().to_owned();
        if source.is_empty() {
            self.feedback = Some("enter an extension source directory".to_owned());
            return None;
        }
        self.dev_path = None;
        Some(ExtensionPickerCommand::InstallDev { source })
    }

    pub(crate) fn replace_records(&mut self, records: Vec<ExtensionRecord>) {
        let selected_id = self.selected_record().map(|record| record.id.clone());
        self.records = records;
        self.sort_records();
        self.refresh();
        if let Some(selected_id) = selected_id
            && let Some(position) = self.visible.iter().position(|index| {
                self.records
                    .get(*index)
                    .is_some_and(|record| record.id == selected_id)
            })
        {
            self.selected = position;
        }
    }

    pub(crate) fn merge_remote(&mut self, remote: Vec<ExtensionRecord>) {
        let local = std::mem::take(&mut self.records);
        let mut merged = remote;
        for installed in local.into_iter().filter(ExtensionRecord::is_installed) {
            if let Some(candidate) = merged
                .iter_mut()
                .find(|candidate| candidate.id == installed.id)
            {
                candidate.installed_version = installed.installed_version;
                candidate.dev = installed.dev;
                candidate.operation = installed.operation;
                if candidate.name.is_empty() {
                    candidate.name = installed.name;
                }
            } else {
                merged.push(installed);
            }
        }
        self.loading = false;
        self.replace_records(merged);
    }

    pub(crate) fn merge_local(&mut self, local: Vec<ExtensionRecord>) {
        let mut merged = std::mem::take(&mut self.records);
        for candidate in &mut merged {
            candidate.installed_version = None;
            candidate.dev = false;
            candidate.operation = None;
        }
        for installed in local {
            if let Some(candidate) = merged
                .iter_mut()
                .find(|candidate| candidate.id == installed.id)
            {
                candidate.installed_version = installed.installed_version;
                candidate.dev = installed.dev;
                candidate.operation = installed.operation;
                if candidate.name.is_empty() {
                    candidate.name = installed.name;
                }
            } else {
                merged.push(installed);
            }
        }
        self.replace_records(merged);
    }

    pub(crate) fn refresh(&mut self) {
        let query = self.prompt.text().trim().to_lowercase();
        self.visible = self
            .records
            .iter()
            .enumerate()
            .filter_map(|(index, record)| {
                let in_scope = match self.scope {
                    ExtensionScope::All => true,
                    ExtensionScope::Installed => record.is_installed(),
                    ExtensionScope::Updates => record.has_update(),
                };
                (in_scope && (query.is_empty() || fuzzy_match(&record.searchable_text(), &query)))
                    .then_some(index)
            })
            .collect();
        self.selected = self.selected.min(self.visible.len().saturating_sub(1));
        self.feedback = None;
    }

    pub(crate) fn cycle_scope(&mut self) {
        self.scope = self.scope.next();
        self.selected = 0;
        self.refresh();
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

    pub(crate) fn selected_record(&self) -> Option<&ExtensionRecord> {
        self.visible
            .get(self.selected)
            .and_then(|index| self.records.get(*index))
    }

    pub(crate) fn primary_command(&self) -> Option<ExtensionPickerCommand> {
        let selected = self.selected_record()?;
        if selected.operation.is_some() {
            return None;
        }
        if selected.dev {
            return Some(ExtensionPickerCommand::RebuildDev {
                id: selected.id.clone(),
            });
        }
        if selected.has_update() {
            Some(ExtensionPickerCommand::Upgrade {
                id: selected.id.clone(),
                version: selected.latest_version.clone(),
            })
        } else if !selected.is_installed() {
            Some(ExtensionPickerCommand::Install {
                id: selected.id.clone(),
            })
        } else {
            None
        }
    }

    pub(crate) fn uninstall_command(&self) -> Option<ExtensionPickerCommand> {
        let selected = self.selected_record()?;
        (selected.is_installed() && selected.operation.is_none()).then(|| {
            ExtensionPickerCommand::Uninstall {
                id: selected.id.clone(),
            }
        })
    }

    pub(crate) fn status(&self, message: Option<&str>) -> (String, usize) {
        let message_prefix = message.map_or_else(String::new, |message| format!("{message}  |  "));
        if let Some(prompt) = self.dev_path.as_ref() {
            let prefix = format!("{message_prefix}Dev extension path: ");
            let cursor = prefix.width().saturating_add(
                prompt
                    .text()
                    .get(..prompt.cursor())
                    .unwrap_or_default()
                    .width(),
            );
            let feedback = self
                .feedback
                .as_deref()
                .map(|feedback| format!("  |  {feedback}"))
                .unwrap_or_default();
            return (
                format!(
                    "{prefix}{}  Enter install  Esc back{feedback}",
                    prompt.text()
                ),
                cursor,
            );
        }
        let prefix = format!("{message_prefix}Extensions [{}]: ", self.scope.label());
        let cursor = prefix.width().saturating_add(
            self.prompt
                .text()
                .get(..self.prompt.cursor())
                .unwrap_or_default()
                .width(),
        );
        let current = if self.visible.is_empty() {
            0
        } else {
            self.selected + 1
        };
        let loading = self.loading.then_some("  · loading registry").unwrap_or("");
        let feedback = self
            .feedback
            .as_deref()
            .map(|feedback| format!("  |  {feedback}"))
            .unwrap_or_default();
        (
            format!(
                "{prefix}{}  {current}/{}{loading}{feedback}  |  Enter install/update/rebuild  Del uninstall  Ctrl-D dev  Tab scope  Ctrl-R reload  Esc close",
                self.prompt.text(),
                self.visible.len()
            ),
            cursor,
        )
    }

    pub(crate) fn overlay(&self) -> OverlaySnapshot {
        const LIMIT: usize = 14;
        if self.dev_path.is_some() {
            return OverlaySnapshot {
                title: " Install Dev Extension ".to_owned(),
                rows: vec![OverlayRow {
                    text: "Enter a directory containing extension.toml; ~ and environment variables are expanded.".to_owned(),
                    enabled: true,
                }],
                selected: None,
            };
        }
        let start = self
            .selected
            .saturating_sub(LIMIT / 2)
            .min(self.visible.len().saturating_sub(LIMIT));
        let end = start.saturating_add(LIMIT).min(self.visible.len());
        OverlaySnapshot {
            title: format!(" Extensions · {} ", self.scope.label()),
            rows: self.visible[start..end]
                .iter()
                .filter_map(|index| self.records.get(*index))
                .map(|record| OverlayRow {
                    text: record.row(),
                    enabled: record.operation.is_none(),
                })
                .collect(),
            selected: (!self.visible.is_empty()).then_some(self.selected.saturating_sub(start)),
        }
    }

    fn sort_records(&mut self) {
        self.records.sort_by(|left, right| {
            right
                .is_installed()
                .cmp(&left.is_installed())
                .then_with(|| right.has_update().cmp(&left.has_update()))
                .then_with(|| compare_names(&left.name, &right.name))
                .then_with(|| left.id.cmp(&right.id))
        });
    }
}

fn compare_names(left: &str, right: &str) -> Ordering {
    left.to_lowercase()
        .cmp(&right.to_lowercase())
        .then_with(|| left.cmp(right))
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

    fn record(id: &str, latest: &str, installed: Option<&str>, dev: bool) -> ExtensionRecord {
        ExtensionRecord {
            id: id.into(),
            name: id.to_owned(),
            latest_version: latest.into(),
            installed_version: installed.map(Arc::from),
            description: None,
            download_count: 0,
            dev,
            operation: None,
            provides: Vec::new(),
        }
    }

    #[test]
    fn semver_updates_do_not_downgrade_or_update_dev_extensions() {
        assert!(record("rust", "2.0.0", Some("1.9.0"), false).has_update());
        assert!(!record("rust", "1.9.0", Some("2.0.0"), false).has_update());
        assert!(!record("rust", "2.0.0", Some("1.9.0"), true).has_update());
    }

    #[test]
    fn remote_results_preserve_installed_state_and_local_only_extensions() {
        let mut picker = ExtensionPickerPrompt::new(vec![
            record("rust", "1.0.0", Some("1.0.0"), false),
            record("local", "0.1.0", Some("0.1.0"), true),
        ]);
        picker.merge_remote(vec![
            record("rust", "2.0.0", None, false),
            record("python", "3.0.0", None, false),
        ]);

        let rust = picker
            .records()
            .iter()
            .find(|record| record.id.as_ref() == "rust")
            .unwrap();
        assert_eq!(rust.installed_version.as_deref(), Some("1.0.0"));
        assert!(rust.has_update());
        assert!(
            picker
                .records()
                .iter()
                .any(|record| record.id.as_ref() == "local" && record.dev)
        );
    }

    #[test]
    fn scope_and_commands_follow_extension_state() {
        let mut picker = ExtensionPickerPrompt::new(vec![
            record("rust", "2.0.0", Some("1.0.0"), false),
            record("python", "3.0.0", None, false),
        ]);
        assert!(matches!(
            picker.primary_command(),
            Some(ExtensionPickerCommand::Upgrade { .. })
        ));
        picker.cycle_scope();
        assert_eq!(picker.scope, ExtensionScope::Installed);
        picker.cycle_scope();
        assert_eq!(picker.scope, ExtensionScope::Updates);
        assert_eq!(picker.visible.len(), 1);
    }

    #[test]
    fn operation_feedback_precedes_help_and_remains_visible_with_a_message() {
        let mut picker = ExtensionPickerPrompt::new(vec![record(
            "beta-2-dev-theme",
            "0.4.3",
            Some("0.4.3"),
            true,
        )]);
        assert_eq!(
            picker.prompt.handle_paste("Beta 2 Dev Theme"),
            PromptAction::Changed
        );
        let feedback = "press Delete again to uninstall beta-2-dev-theme";
        picker.set_feedback(feedback);

        let (status, _) = picker.status(Some("user settings reloaded"));
        let first_160_columns = status.chars().take(160).collect::<String>();
        assert!(first_160_columns.contains(feedback));
        assert!(status.find(feedback) < status.find("Enter install/update/rebuild"));
    }

    #[test]
    fn dev_extensions_can_be_installed_rebuilt_and_uninstalled() {
        let mut picker =
            ExtensionPickerPrompt::new(vec![record("console-theme", "0.1.0", Some("0.1.0"), true)]);
        assert!(matches!(
            picker.primary_command(),
            Some(ExtensionPickerCommand::RebuildDev { .. })
        ));
        assert!(matches!(
            picker.uninstall_command(),
            Some(ExtensionPickerCommand::Uninstall { .. })
        ));

        picker.begin_dev_install();
        assert!(picker.is_entering_dev_path());
        assert_eq!(
            picker.handle_dev_paste("~/src/console-theme\nignored"),
            PromptAction::Changed
        );
        assert_eq!(
            picker.dev_install_command(),
            Some(ExtensionPickerCommand::InstallDev {
                source: "~/src/console-theme".to_owned()
            })
        );
        assert!(!picker.is_entering_dev_path());
    }
}
