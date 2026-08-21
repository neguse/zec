//! Single-line terminal prompt state for terminal-owned commands.
//!
//! Zed owns the document editor. This module only edits text shown in terminal
//! chrome (currently search/replace, Save As, Open, and Go to line) and translates
//! prompt-local keys into commands for the caller.
//!
//! The cursor is always a UTF-8 byte offset on a `char` boundary. Movement and
//! deletion operate on Unicode scalar values, not grapheme clusters. Paste is
//! intentionally single-line: only the text before the first `\r` or `\n` is
//! inserted. Search history, selection, undo, and search options are outside
//! this minimal prompt. Search replacement transactions remain owned by Zed.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// The result of handling one key while a line prompt is active.
///
/// Every key passed to [`LinePrompt::handle_key`] is consumed by the prompt.
/// In particular, [`Self::Ignored`] must not be forwarded to the document
/// editor. Callers that support global shortcuts such as quit or save should
/// handle those before calling this method.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PromptAction {
    /// The prompt text changed.
    Changed,
    /// Only the prompt cursor moved.
    CursorMoved,
    /// Submit with Enter.
    Submit,
    /// Submit with Shift-Enter.
    AlternateSubmit,
    /// Move to the next item with Down.
    Next,
    /// Move to the previous item with Up.
    Previous,
    /// Close the prompt.
    Cancel,
    /// Consume the key without changing prompt state.
    Ignored,
}

/// Editable state for a terminal-owned single-line prompt.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LinePrompt {
    text: String,
    cursor: usize,
}

impl LinePrompt {
    /// Creates an empty prompt with its cursor at byte offset zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a prompt containing `text`, with its cursor at the end.
    #[cfg(test)]
    pub fn with_text(text: impl Into<String>) -> Self {
        let text = text.into();
        Self {
            cursor: text.len(),
            text,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the cursor as a UTF-8 byte offset into [`Self::text`].
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Handles a key event without performing any document-editor action.
    pub fn handle_key(&mut self, event: &KeyEvent) -> PromptAction {
        if event.kind == KeyEventKind::Release {
            return PromptAction::Ignored;
        }

        let action = match event.code {
            KeyCode::Enter if event.modifiers == KeyModifiers::NONE => PromptAction::Submit,
            KeyCode::Enter if event.modifiers == KeyModifiers::SHIFT => {
                PromptAction::AlternateSubmit
            }
            KeyCode::Down if event.modifiers == KeyModifiers::NONE => PromptAction::Next,
            KeyCode::Up if event.modifiers == KeyModifiers::NONE => PromptAction::Previous,
            KeyCode::Esc if event.modifiers == KeyModifiers::NONE => PromptAction::Cancel,
            KeyCode::Backspace if event.modifiers == KeyModifiers::NONE => self.backspace(),
            KeyCode::Delete if event.modifiers == KeyModifiers::NONE => self.delete(),
            KeyCode::Left if event.modifiers == KeyModifiers::NONE => self.move_left(),
            KeyCode::Right if event.modifiers == KeyModifiers::NONE => self.move_right(),
            KeyCode::Home if event.modifiers == KeyModifiers::NONE => self.move_home(),
            KeyCode::End if event.modifiers == KeyModifiers::NONE => self.move_end(),
            KeyCode::Char(character)
                if !character.is_control()
                    && !event.modifiers.intersects(
                        KeyModifiers::CONTROL
                            | KeyModifiers::ALT
                            | KeyModifiers::SUPER
                            | KeyModifiers::HYPER
                            | KeyModifiers::META,
                    ) =>
            {
                self.text.insert(self.cursor, character);
                self.cursor += character.len_utf8();
                PromptAction::Changed
            }
            _ => PromptAction::Ignored,
        };

        debug_assert!(self.cursor <= self.text.len());
        debug_assert!(self.text.is_char_boundary(self.cursor));
        action
    }

    /// Inserts the first logical line of a bracketed-paste payload.
    pub fn handle_paste(&mut self, text: &str) -> PromptAction {
        let line_end = text
            .char_indices()
            .find_map(|(index, character)| matches!(character, '\r' | '\n').then_some(index))
            .unwrap_or(text.len());
        let line = &text[..line_end];
        if line.is_empty() {
            return PromptAction::Ignored;
        }

        self.text.insert_str(self.cursor, line);
        self.cursor += line.len();
        debug_assert!(self.text.is_char_boundary(self.cursor));
        PromptAction::Changed
    }

    fn backspace(&mut self) -> PromptAction {
        let Some(previous) = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
        else {
            return PromptAction::Ignored;
        };

        self.text.drain(previous..self.cursor);
        self.cursor = previous;
        PromptAction::Changed
    }

    fn delete(&mut self) -> PromptAction {
        let Some(character) = self.text[self.cursor..].chars().next() else {
            return PromptAction::Ignored;
        };

        let end = self.cursor + character.len_utf8();
        self.text.drain(self.cursor..end);
        PromptAction::Changed
    }

    fn move_left(&mut self) -> PromptAction {
        let Some(previous) = self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
        else {
            return PromptAction::Ignored;
        };

        self.cursor = previous;
        PromptAction::CursorMoved
    }

    fn move_right(&mut self) -> PromptAction {
        let Some(character) = self.text[self.cursor..].chars().next() else {
            return PromptAction::Ignored;
        };

        self.cursor += character.len_utf8();
        PromptAction::CursorMoved
    }

    fn move_home(&mut self) -> PromptAction {
        if self.cursor == 0 {
            PromptAction::Ignored
        } else {
            self.cursor = 0;
            PromptAction::CursorMoved
        }
    }

    fn move_end(&mut self) -> PromptAction {
        if self.cursor == self.text.len() {
            PromptAction::Ignored
        } else {
            self.cursor = self.text.len();
            PromptAction::CursorMoved
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn modified_key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn assert_cursor_is_valid(prompt: &LinePrompt) {
        assert!(prompt.cursor() <= prompt.text().len());
        assert!(prompt.text().is_char_boundary(prompt.cursor()));
    }

    #[test]
    fn inserts_printable_characters_at_the_cursor() {
        let mut prompt = LinePrompt::with_text("ac");
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Home)),
            PromptAction::CursorMoved
        );
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Right)),
            PromptAction::CursorMoved
        );
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Char('b'))),
            PromptAction::Changed
        );

        assert_eq!(prompt.text(), "abc");
        assert_eq!(prompt.cursor(), 2);
        assert_cursor_is_valid(&prompt);
    }

    #[test]
    fn moves_and_deletes_at_utf8_char_boundaries() {
        let mut prompt = LinePrompt::with_text("日本🦀語");

        assert_eq!(prompt.cursor(), "日本🦀語".len());
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Left)),
            PromptAction::CursorMoved
        );
        assert_eq!(prompt.cursor(), "日本🦀".len());
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Backspace)),
            PromptAction::Changed
        );
        assert_eq!(prompt.text(), "日本語");
        assert_eq!(prompt.cursor(), "日本".len());
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Delete)),
            PromptAction::Changed
        );
        assert_eq!(prompt.text(), "日本");
        assert_cursor_is_valid(&prompt);
    }

    #[test]
    fn combining_marks_are_individual_scalar_positions() {
        let mut prompt = LinePrompt::with_text("e\u{301}x");

        prompt.handle_key(&key(KeyCode::Home));
        prompt.handle_key(&key(KeyCode::Right));
        assert_eq!(prompt.cursor(), 1);
        prompt.handle_key(&key(KeyCode::Right));
        assert_eq!(prompt.cursor(), "e\u{301}".len());
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Backspace)),
            PromptAction::Changed
        );

        assert_eq!(prompt.text(), "ex");
        assert_eq!(prompt.cursor(), 1);
        assert_cursor_is_valid(&prompt);
    }

    #[test]
    fn walks_emoji_sequences_without_breaking_utf8() {
        let mut prompt = LinePrompt::with_text("👩\u{200d}💻!");

        for _ in 0..4 {
            prompt.handle_key(&key(KeyCode::Left));
            assert_cursor_is_valid(&prompt);
        }
        assert_eq!(prompt.cursor(), 0);

        for _ in 0..4 {
            prompt.handle_key(&key(KeyCode::Right));
            assert_cursor_is_valid(&prompt);
        }
        assert_eq!(prompt.cursor(), prompt.text().len());
    }

    #[test]
    fn home_end_and_boundary_edits_report_no_op() {
        let mut prompt = LinePrompt::with_text("abc");

        assert_eq!(prompt.handle_key(&key(KeyCode::End)), PromptAction::Ignored);
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Delete)),
            PromptAction::Ignored
        );
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Home)),
            PromptAction::CursorMoved
        );
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Home)),
            PromptAction::Ignored
        );
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Backspace)),
            PromptAction::Ignored
        );
    }

    #[test]
    fn reports_submit_navigation_and_cancel_without_mutating_text() {
        let mut prompt = LinePrompt::with_text("needle");

        assert_eq!(
            prompt.handle_key(&key(KeyCode::Enter)),
            PromptAction::Submit
        );
        assert_eq!(
            prompt.handle_key(&modified_key(KeyCode::Enter, KeyModifiers::SHIFT)),
            PromptAction::AlternateSubmit
        );
        assert_eq!(prompt.handle_key(&key(KeyCode::Down)), PromptAction::Next);
        assert_eq!(prompt.handle_key(&key(KeyCode::Up)), PromptAction::Previous);
        assert_eq!(prompt.handle_key(&key(KeyCode::Esc)), PromptAction::Cancel);
        assert_eq!(prompt.text(), "needle");
        assert_eq!(prompt.cursor(), "needle".len());
    }

    #[test]
    fn ignores_release_events_and_modified_shortcuts() {
        let mut prompt = LinePrompt::new();
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );

        assert_eq!(prompt.handle_key(&release), PromptAction::Ignored);
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SUPER,
            KeyModifiers::HYPER,
            KeyModifiers::META,
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ] {
            assert_eq!(
                prompt.handle_key(&modified_key(KeyCode::Char('x'), modifiers)),
                PromptAction::Ignored
            );
        }
        assert_eq!(
            prompt.handle_key(&modified_key(KeyCode::Enter, KeyModifiers::CONTROL)),
            PromptAction::Ignored
        );
        assert_eq!(prompt.text(), "");
    }

    #[test]
    fn shift_does_not_block_printable_characters() {
        let mut prompt = LinePrompt::new();

        assert_eq!(
            prompt.handle_key(&modified_key(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            PromptAction::Changed
        );
        assert_eq!(prompt.text(), "A");
        assert_eq!(prompt.cursor(), 1);
    }

    #[test]
    fn accepts_repeat_events_for_held_editing_keys() {
        let mut prompt = LinePrompt::with_text("ab");
        let repeat =
            KeyEvent::new_with_kind(KeyCode::Backspace, KeyModifiers::NONE, KeyEventKind::Repeat);

        assert_eq!(prompt.handle_key(&repeat), PromptAction::Changed);
        assert_eq!(prompt.text(), "a");
    }

    #[test]
    fn paste_inserts_only_the_first_line_at_the_cursor() {
        let mut prompt = LinePrompt::with_text("ab");
        prompt.handle_key(&key(KeyCode::Left));

        assert_eq!(
            prompt.handle_paste("日本🦀\r\nignored"),
            PromptAction::Changed
        );
        assert_eq!(prompt.text(), "a日本🦀b");
        assert_eq!(prompt.cursor(), "a日本🦀".len());
        assert_cursor_is_valid(&prompt);
    }

    #[test]
    fn empty_first_paste_line_is_ignored() {
        let mut prompt = LinePrompt::with_text("unchanged");

        assert_eq!(prompt.handle_paste("\nsecond line"), PromptAction::Ignored);
        assert_eq!(prompt.text(), "unchanged");
        assert_eq!(prompt.cursor(), "unchanged".len());
    }

    #[test]
    fn paste_preserves_combining_characters_and_tabs() {
        let mut prompt = LinePrompt::new();

        assert_eq!(prompt.handle_paste("e\u{301}\t🦀"), PromptAction::Changed);
        assert_eq!(prompt.text(), "e\u{301}\t🦀");
        assert_eq!(prompt.cursor(), prompt.text().len());
        assert_cursor_is_valid(&prompt);
    }
}
