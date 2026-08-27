//! Terminal projections for Zed debug scenarios and live DAP sessions.

use project::DebugScenarioContext;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Clear, Widget},
};
use task::DebugScenario;
use unicode_width::UnicodeWidthStr;

use crate::{
    prompt::LinePrompt,
    render::{OverlayRow, OverlaySnapshot},
    tabs::Direction,
};

#[derive(Debug)]
pub(crate) struct DebugScenarioPicker {
    pub(crate) prompt: LinePrompt,
    scenarios: Vec<(DebugScenario, DebugScenarioContext)>,
    visible: Vec<usize>,
    selected: usize,
    feedback: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct DebugReplPrompt {
    pub(crate) prompt: LinePrompt,
    feedback: Option<String>,
}

impl DebugReplPrompt {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn set_feedback(&mut self, feedback: impl Into<String>) {
        self.feedback = Some(feedback.into());
    }

    pub(crate) fn status(&self, message: Option<&str>) -> (String, usize) {
        let message_prefix = message.map_or_else(String::new, |message| format!("{message}  |  "));
        let prefix = format!("{message_prefix}Debug console: ");
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
            .map(|feedback| format!("  |  {feedback}"))
            .unwrap_or_default();
        (
            format!(
                "{prefix}{}  Enter evaluate  Esc cancel{suffix}",
                self.prompt.text()
            ),
            cursor,
        )
    }
}

impl DebugScenarioPicker {
    pub(crate) fn new(scenarios: Vec<(DebugScenario, DebugScenarioContext)>) -> Self {
        let mut this = Self {
            prompt: LinePrompt::new(),
            scenarios,
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
            .scenarios
            .iter()
            .enumerate()
            .filter_map(|(index, (scenario, _))| {
                let request = scenario
                    .config
                    .get("request")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown");
                let searchable =
                    format!("{} {} {request}", scenario.label, scenario.adapter).to_lowercase();
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

    pub(crate) fn selected_scenario(&self) -> Option<(DebugScenario, DebugScenarioContext)> {
        self.visible
            .get(self.selected)
            .and_then(|index| self.scenarios.get(*index))
            .cloned()
    }

    pub(crate) fn set_feedback(&mut self, feedback: impl Into<String>) {
        self.feedback = Some(feedback.into());
    }

    pub(crate) fn status(&self, message: Option<&str>) -> (String, usize) {
        let message_prefix = message.map_or_else(String::new, |message| format!("{message}  |  "));
        let prefix = format!("{message_prefix}Debug: ");
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
            .map(|feedback| format!("  |  {feedback}"))
            .unwrap_or_default();
        let current = if self.visible.is_empty() {
            0
        } else {
            self.selected + 1
        };
        (
            format!(
                "{prefix}{}  {current}/{}  Enter start  ↑/↓ select  Esc cancel{suffix}",
                self.prompt.text(),
                self.visible.len()
            ),
            cursor,
        )
    }

    pub(crate) fn overlay(&self) -> OverlaySnapshot {
        const LIMIT: usize = 12;
        let start = self
            .selected
            .saturating_sub(LIMIT / 2)
            .min(self.visible.len().saturating_sub(LIMIT));
        let end = start.saturating_add(LIMIT).min(self.visible.len());
        OverlaySnapshot {
            title: " Debug Configurations ".to_owned(),
            rows: self.visible[start..end]
                .iter()
                .filter_map(|index| self.scenarios.get(*index))
                .map(|(scenario, _)| {
                    let request = scenario
                        .config
                        .get("request")
                        .and_then(|value| value.as_str())
                        .unwrap_or("unknown");
                    OverlayRow {
                        text: format!("{}  · {}  · {request}", scenario.label, scenario.adapter),
                        enabled: true,
                    }
                })
                .collect(),
            selected: (!self.visible.is_empty()).then_some(self.selected.saturating_sub(start)),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DebugThreadRow {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) status: String,
    pub(crate) selected: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct DebuggerPanelSnapshot {
    pub(crate) label: String,
    pub(crate) adapter: String,
    pub(crate) state: String,
    pub(crate) threads: Vec<DebugThreadRow>,
    pub(crate) frames: Vec<String>,
    pub(crate) variables: Vec<String>,
    pub(crate) breakpoints: Vec<String>,
    pub(crate) console: Vec<String>,
}

pub(crate) struct DebuggerPanelWidget<'a> {
    snapshot: &'a DebuggerPanelSnapshot,
    focused: bool,
}

impl<'a> DebuggerPanelWidget<'a> {
    pub(crate) fn new(snapshot: &'a DebuggerPanelSnapshot, focused: bool) -> Self {
        Self { snapshot, focused }
    }
}

impl Widget for DebuggerPanelWidget<'_> {
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
            .title(format!(
                " Debugger · {} · {} · {} ",
                self.snapshot.label, self.snapshot.adapter, self.snapshot.state
            ))
            .border_style(border_style);
        let inner = block.inner(area);
        block.render(area, buffer);
        if inner.is_empty() {
            return;
        }

        let mut rows = Vec::<(String, Style)>::new();
        push_section(&mut rows, "Threads", !self.snapshot.threads.is_empty());
        for thread in &self.snapshot.threads {
            rows.push((
                format!(
                    "{} #{} {} · {}",
                    if thread.selected { "›" } else { " " },
                    thread.id,
                    thread.name,
                    thread.status
                ),
                if thread.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                },
            ));
        }
        push_section(&mut rows, "Frames", !self.snapshot.frames.is_empty());
        rows.extend(
            self.snapshot
                .frames
                .iter()
                .map(|row| (format!("  {row}"), Style::default())),
        );
        push_section(&mut rows, "Variables", !self.snapshot.variables.is_empty());
        rows.extend(
            self.snapshot
                .variables
                .iter()
                .map(|row| (format!("  {row}"), Style::default().fg(Color::Yellow))),
        );
        push_section(
            &mut rows,
            "Breakpoints",
            !self.snapshot.breakpoints.is_empty(),
        );
        rows.extend(
            self.snapshot
                .breakpoints
                .iter()
                .map(|row| (format!("  {row}"), Style::default().fg(Color::Red))),
        );
        push_section(&mut rows, "Console / REPL", true);
        if self.snapshot.console.is_empty() {
            rows.push((
                "  (no output)".to_owned(),
                Style::default().fg(Color::DarkGray),
            ));
        } else {
            rows.extend(
                self.snapshot
                    .console
                    .iter()
                    .map(|row| (format!("  {row}"), Style::default().fg(Color::Green))),
            );
        }

        let height = usize::from(inner.height);
        let start = rows.len().saturating_sub(height);
        for (offset, (row, style)) in rows.into_iter().skip(start).enumerate() {
            let Ok(offset) = u16::try_from(offset) else {
                break;
            };
            buffer.set_stringn(
                inner.x,
                inner.y.saturating_add(offset),
                sanitize_line(&row),
                usize::from(inner.width),
                style,
            );
        }
    }
}

fn push_section(rows: &mut Vec<(String, Style)>, title: &str, show: bool) {
    if show {
        rows.push((
            title.to_owned(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    }
}

fn sanitize_line(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() || *character == '\t')
        .collect()
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
    use ratatui::buffer::Buffer;
    use serde_json::json;

    fn scenario(label: &str, adapter: &str) -> DebugScenario {
        DebugScenario {
            adapter: adapter.into(),
            label: label.into(),
            build: None,
            config: json!({ "request": "launch", "program": "fixture" }),
            tcp_connection: None,
        }
    }

    #[test]
    fn debug_picker_filters_and_projects_zed_scenarios() {
        let mut picker = DebugScenarioPicker::new(vec![
            (
                scenario("Rust binary", "CodeLLDB"),
                DebugScenarioContext::default(),
            ),
            (
                scenario("Python app", "Debugpy"),
                DebugScenarioContext::default(),
            ),
        ]);
        assert_eq!(picker.overlay().rows.len(), 2);
        picker.prompt.handle_paste("pydbg");
        picker.refresh();
        assert_eq!(picker.selected_scenario().unwrap().0.label, "Python app");
        assert!(picker.overlay().rows[0].text.contains("Debugpy"));
    }

    #[test]
    fn debugger_widget_bounds_live_state_and_sanitizes_output() {
        let snapshot = DebuggerPanelSnapshot {
            label: "fixture".to_owned(),
            adapter: "DAP".to_owned(),
            state: "stopped".to_owned(),
            threads: vec![DebugThreadRow {
                id: 1,
                name: "main".to_owned(),
                status: "Stopped".to_owned(),
                selected: true,
            }],
            frames: vec!["main · fixture.rs:7".to_owned()],
            variables: vec!["answer = 42".to_owned()],
            breakpoints: vec!["fixture.rs:7".to_owned()],
            console: vec!["ready\u{1b}[31m".to_owned()],
        };
        let area = Rect::new(0, 0, 60, 12);
        let mut buffer = Buffer::empty(area);
        DebuggerPanelWidget::new(&snapshot, true).render(area, &mut buffer);
        let rendered = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Debugger"));
        assert!(rendered.contains("answer = 42"));
        assert!(!rendered.contains('\u{1b}'));
    }
}
