//! Theme and icon-theme selection projected into a bounded terminal picker.

use unicode_width::UnicodeWidthStr;

use crate::{
    prompt::LinePrompt,
    render::{OverlayRow, OverlaySnapshot},
    tabs::Direction,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ThemePickerKind {
    Color,
    Icon,
}

impl ThemePickerKind {
    fn title(self) -> &'static str {
        match self {
            Self::Color => "Themes",
            Self::Icon => "Icon Themes",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ThemeChoice {
    pub(crate) name: String,
    pub(crate) appearance: String,
    pub(crate) active: bool,
}

#[derive(Debug)]
pub(crate) struct ThemePickerPrompt {
    pub(crate) prompt: LinePrompt,
    kind: ThemePickerKind,
    choices: Vec<ThemeChoice>,
    visible: Vec<usize>,
    selected: usize,
    feedback: Option<String>,
}

impl ThemePickerPrompt {
    pub(crate) fn new(kind: ThemePickerKind, mut choices: Vec<ThemeChoice>) -> Self {
        choices.sort_by(|left, right| {
            right
                .active
                .cmp(&left.active)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                .then_with(|| left.name.cmp(&right.name))
        });
        let mut this = Self {
            prompt: LinePrompt::new(),
            kind,
            choices,
            visible: Vec::new(),
            selected: 0,
            feedback: None,
        };
        this.refresh();
        this
    }

    pub(crate) fn kind(&self) -> ThemePickerKind {
        self.kind
    }

    pub(crate) fn refresh(&mut self) {
        let query = self.prompt.text().trim().to_lowercase();
        self.visible = self
            .choices
            .iter()
            .enumerate()
            .filter_map(|(index, choice)| {
                let searchable = format!("{} {}", choice.name, choice.appearance).to_lowercase();
                (query.is_empty() || fuzzy_match(&searchable, &query)).then_some(index)
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

    pub(crate) fn selected_choice(&self) -> Option<&ThemeChoice> {
        self.visible
            .get(self.selected)
            .and_then(|index| self.choices.get(*index))
    }

    pub(crate) fn set_feedback(&mut self, feedback: impl Into<String>) {
        self.feedback = Some(feedback.into());
    }

    pub(crate) fn status(&self, message: Option<&str>) -> (String, usize) {
        let message_prefix = message.map_or_else(String::new, |message| format!("{message}  |  "));
        let prefix = format!("{message_prefix}{}: ", self.kind.title());
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
        let feedback = self
            .feedback
            .as_deref()
            .map(|feedback| format!("  |  {feedback}"))
            .unwrap_or_default();
        (
            format!(
                "{prefix}{}  {current}/{}  Enter apply  ↑/↓ select  Esc cancel{feedback}",
                self.prompt.text(),
                self.visible.len()
            ),
            cursor,
        )
    }

    pub(crate) fn overlay(&self) -> OverlaySnapshot {
        const LIMIT: usize = 14;
        let start = self
            .selected
            .saturating_sub(LIMIT / 2)
            .min(self.visible.len().saturating_sub(LIMIT));
        let end = start.saturating_add(LIMIT).min(self.visible.len());
        OverlaySnapshot {
            title: format!(" {} ", self.kind.title()),
            rows: self.visible[start..end]
                .iter()
                .filter_map(|index| self.choices.get(*index))
                .map(|choice| OverlayRow {
                    text: format!(
                        "{}{}  · {}",
                        if choice.active { "● " } else { "  " },
                        choice.name,
                        choice.appearance
                    ),
                    enabled: true,
                })
                .collect(),
            selected: (!self.visible.is_empty()).then_some(self.selected.saturating_sub(start)),
        }
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
    fn active_theme_is_first_and_filtering_is_ordered() {
        let mut picker = ThemePickerPrompt::new(
            ThemePickerKind::Color,
            vec![
                ThemeChoice {
                    name: "Zed Light".to_owned(),
                    appearance: "light".to_owned(),
                    active: false,
                },
                ThemeChoice {
                    name: "One Dark".to_owned(),
                    appearance: "dark".to_owned(),
                    active: true,
                },
            ],
        );
        assert_eq!(picker.selected_choice().unwrap().name, "One Dark");
        picker.prompt.handle_paste("zl");
        picker.refresh();
        assert_eq!(picker.selected_choice().unwrap().name, "Zed Light");
    }
}
