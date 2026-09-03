//! Prompts, pickers, and confirmations: the one mechanism for transient
//! input owners stacked above the workspace focus.

use std::path::PathBuf;

use unicode_width::UnicodeWidthStr as _;

use crate::{
    app::command::Command,
    terminal::{picker::PickerList, prompt::LinePrompt, render::OverlaySnapshot},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PromptTarget {
    /// `overwrite` holds the path a first submit refused to clobber; the
    /// same path submitted again overwrites it.
    SaveAs {
        overwrite: Option<PathBuf>,
    },
    OpenFile,
    ProjectSearch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickerPayload {
    Command(Command),
    /// A file to open in a tab.
    Path(PathBuf),
    /// A file to open with the caret at a buffer point; `column` is a
    /// byte offset in the line.
    Location {
        path: PathBuf,
        row: u32,
        column: u32,
    },
}

/// Who refreshes a picker's entries when its query changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PickerOwner {
    /// The entries are fixed; the list filters them itself.
    Palette,
    QuickOpen,
    /// Fixed entries too: the search ran once, the list filters its hits.
    ProjectSearch,
}

#[derive(Debug)]
pub enum Overlay {
    Prompt {
        label: &'static str,
        line: LinePrompt,
        target: PromptTarget,
        feedback: Option<String>,
    },
    Picker {
        title: &'static str,
        query: LinePrompt,
        list: PickerList<PickerPayload>,
        owner: PickerOwner,
    },
    /// The same command again executes; any other input dismisses it.
    Confirm { message: String, command: Command },
}

/// What the top overlay contributes to the focused pane.
#[derive(Debug)]
pub enum Presentation {
    None,
    /// A message shown ahead of the normal status row.
    Message(String),
    /// The status row is the overlay's input line.
    Input {
        status: String,
        cursor_column: usize,
        overlay: Option<OverlaySnapshot>,
    },
}

#[derive(Debug, Default)]
pub struct Overlays {
    stack: Vec<Overlay>,
}

impl Overlays {
    pub fn push(&mut self, overlay: Overlay) {
        self.stack.push(overlay);
    }

    pub fn pop(&mut self) -> Option<Overlay> {
        self.stack.pop()
    }

    pub fn top_mut(&mut self) -> Option<&mut Overlay> {
        self.stack.last_mut()
    }

    /// Whether a prompt or picker currently owns keyboard input.
    pub fn owns_input(&self) -> bool {
        matches!(
            self.stack.last(),
            Some(Overlay::Prompt { .. } | Overlay::Picker { .. })
        )
    }

    /// Consumes a pending confirmation for `command`, returning whether the
    /// command was confirmed. Any other pending confirmation is dismissed.
    pub fn take_confirmation(&mut self, command: Command) -> bool {
        match self.stack.last() {
            Some(Overlay::Confirm {
                command: pending, ..
            }) => {
                let confirmed = *pending == command;
                self.stack.pop();
                confirmed
            }
            _ => false,
        }
    }

    pub fn dismiss_confirmation(&mut self) {
        if matches!(self.stack.last(), Some(Overlay::Confirm { .. })) {
            self.stack.pop();
        }
    }

    /// Closes every overlay; they are all bound to the active item.
    pub fn clear(&mut self) {
        self.stack.clear();
    }

    pub fn presentation(&self, row_budget: usize) -> Presentation {
        match self.stack.last() {
            None => Presentation::None,
            Some(Overlay::Prompt {
                label,
                line,
                feedback,
                ..
            }) => {
                let prefix = format!("{label}: ");
                let mut status = format!("{prefix}{}", line.text());
                if let Some(feedback) = feedback {
                    status.push_str("  |  ");
                    status.push_str(feedback);
                }
                Presentation::Input {
                    status,
                    cursor_column: prefix.width() + line.text()[..line.cursor()].width(),
                    overlay: None,
                }
            }
            Some(Overlay::Picker {
                title, query, list, ..
            }) => {
                let prefix = format!("{title}: ");
                Presentation::Input {
                    status: format!("{prefix}{}", query.text()),
                    cursor_column: prefix.width() + query.text()[..query.cursor()].width(),
                    overlay: Some(list.snapshot(title, row_budget)),
                }
            }
            Some(Overlay::Confirm { message, .. }) => Presentation::Message(message.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_is_consumed_only_by_the_same_command() {
        let mut overlays = Overlays::default();
        overlays.push(Overlay::Confirm {
            message: "sure?".to_owned(),
            command: Command::Quit,
        });
        assert!(!overlays.take_confirmation(Command::Save));
        assert!(!overlays.take_confirmation(Command::Quit));

        overlays.push(Overlay::Confirm {
            message: "sure?".to_owned(),
            command: Command::Quit,
        });
        assert!(overlays.take_confirmation(Command::Quit));
        assert!(!overlays.owns_input());
    }

    #[test]
    fn prompt_presentation_places_the_cursor_in_terminal_cells() {
        let mut overlays = Overlays::default();
        overlays.push(Overlay::Prompt {
            label: "Save as",
            line: LinePrompt::with_text("日本"),
            target: PromptTarget::SaveAs { overwrite: None },
            feedback: Some("exists".to_owned()),
        });
        let Presentation::Input {
            status,
            cursor_column,
            overlay,
        } = overlays.presentation(3)
        else {
            panic!("prompt must own the status row");
        };
        assert_eq!(status, "Save as: 日本  |  exists");
        assert_eq!(cursor_column, "Save as: ".len() + 4);
        assert!(overlay.is_none());
        assert!(overlays.owns_input());
    }
}
