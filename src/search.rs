//! Single-line terminal prompt state for buffer search.
//!
//! The document search itself remains owned by Zed. This module only edits the
//! query shown in the terminal chrome and translates prompt-local keys into
//! commands for the caller.
//!
//! The cursor is always a UTF-8 byte offset on a `char` boundary. Movement and
//! deletion operate on Unicode scalar values, not grapheme clusters. Paste is
//! intentionally single-line: only the text before the first `\r` or `\n` is
//! inserted. Search history, selection, undo, and search options are outside
//! this minimal prompt.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// The result of handling one key while the search prompt is active.
///
/// Every key passed to [`SearchPrompt::handle_key`] is consumed by the prompt.
/// In particular, [`Self::Ignored`] must not be forwarded to the document
/// editor. Callers that support global shortcuts such as quit or save should
/// handle those before calling this method.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchPromptAction {
    /// The query changed and matches should be recomputed.
    QueryChanged,
    /// Only the query cursor moved; no search is necessary.
    CursorMoved,
    /// Activate the next match.
    NextMatch,
    /// Activate the previous match.
    PreviousMatch,
    /// Close the prompt.
    Cancel,
    /// Consume the key without changing prompt state.
    Ignored,
}

/// Editable state for the terminal's single-line search prompt.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SearchPrompt {
    query: String,
    cursor: usize,
}

impl SearchPrompt {
    /// Creates an empty prompt with its cursor at byte offset zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a prompt containing `query`, with its cursor at the end.
    #[cfg(test)]
    pub fn with_query(query: impl Into<String>) -> Self {
        let query = query.into();
        Self {
            cursor: query.len(),
            query,
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Returns the cursor as a UTF-8 byte offset into [`Self::query`].
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Handles a key event without performing any document-editor action.
    pub fn handle_key(&mut self, event: &KeyEvent) -> SearchPromptAction {
        if event.kind == KeyEventKind::Release {
            return SearchPromptAction::Ignored;
        }

        let action = match event.code {
            KeyCode::Enter if event.modifiers == KeyModifiers::NONE => {
                SearchPromptAction::NextMatch
            }
            KeyCode::Enter if event.modifiers == KeyModifiers::SHIFT => {
                SearchPromptAction::PreviousMatch
            }
            KeyCode::Down if event.modifiers == KeyModifiers::NONE => SearchPromptAction::NextMatch,
            KeyCode::Up if event.modifiers == KeyModifiers::NONE => {
                SearchPromptAction::PreviousMatch
            }
            KeyCode::Esc if event.modifiers == KeyModifiers::NONE => SearchPromptAction::Cancel,
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
                self.query.insert(self.cursor, character);
                self.cursor += character.len_utf8();
                SearchPromptAction::QueryChanged
            }
            _ => SearchPromptAction::Ignored,
        };

        debug_assert!(self.cursor <= self.query.len());
        debug_assert!(self.query.is_char_boundary(self.cursor));
        action
    }

    /// Inserts the first logical line of a bracketed-paste payload.
    pub fn handle_paste(&mut self, text: &str) -> SearchPromptAction {
        let line_end = text
            .char_indices()
            .find_map(|(index, character)| matches!(character, '\r' | '\n').then_some(index))
            .unwrap_or(text.len());
        let line = &text[..line_end];
        if line.is_empty() {
            return SearchPromptAction::Ignored;
        }

        self.query.insert_str(self.cursor, line);
        self.cursor += line.len();
        debug_assert!(self.query.is_char_boundary(self.cursor));
        SearchPromptAction::QueryChanged
    }

    fn backspace(&mut self) -> SearchPromptAction {
        let Some(previous) = self.query[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
        else {
            return SearchPromptAction::Ignored;
        };

        self.query.drain(previous..self.cursor);
        self.cursor = previous;
        SearchPromptAction::QueryChanged
    }

    fn delete(&mut self) -> SearchPromptAction {
        let Some(character) = self.query[self.cursor..].chars().next() else {
            return SearchPromptAction::Ignored;
        };

        let end = self.cursor + character.len_utf8();
        self.query.drain(self.cursor..end);
        SearchPromptAction::QueryChanged
    }

    fn move_left(&mut self) -> SearchPromptAction {
        let Some(previous) = self.query[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
        else {
            return SearchPromptAction::Ignored;
        };

        self.cursor = previous;
        SearchPromptAction::CursorMoved
    }

    fn move_right(&mut self) -> SearchPromptAction {
        let Some(character) = self.query[self.cursor..].chars().next() else {
            return SearchPromptAction::Ignored;
        };

        self.cursor += character.len_utf8();
        SearchPromptAction::CursorMoved
    }

    fn move_home(&mut self) -> SearchPromptAction {
        if self.cursor == 0 {
            SearchPromptAction::Ignored
        } else {
            self.cursor = 0;
            SearchPromptAction::CursorMoved
        }
    }

    fn move_end(&mut self) -> SearchPromptAction {
        if self.cursor == self.query.len() {
            SearchPromptAction::Ignored
        } else {
            self.cursor = self.query.len();
            SearchPromptAction::CursorMoved
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

    fn assert_cursor_is_valid(prompt: &SearchPrompt) {
        assert!(prompt.cursor() <= prompt.query().len());
        assert!(prompt.query().is_char_boundary(prompt.cursor()));
    }

    #[test]
    fn inserts_printable_characters_at_the_cursor() {
        let mut prompt = SearchPrompt::with_query("ac");
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Home)),
            SearchPromptAction::CursorMoved
        );
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Right)),
            SearchPromptAction::CursorMoved
        );
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Char('b'))),
            SearchPromptAction::QueryChanged
        );

        assert_eq!(prompt.query(), "abc");
        assert_eq!(prompt.cursor(), 2);
        assert_cursor_is_valid(&prompt);
    }

    #[test]
    fn moves_and_deletes_at_utf8_char_boundaries() {
        let mut prompt = SearchPrompt::with_query("日本🦀語");

        assert_eq!(prompt.cursor(), "日本🦀語".len());
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Left)),
            SearchPromptAction::CursorMoved
        );
        assert_eq!(prompt.cursor(), "日本🦀".len());
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Backspace)),
            SearchPromptAction::QueryChanged
        );
        assert_eq!(prompt.query(), "日本語");
        assert_eq!(prompt.cursor(), "日本".len());
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Delete)),
            SearchPromptAction::QueryChanged
        );
        assert_eq!(prompt.query(), "日本");
        assert_cursor_is_valid(&prompt);
    }

    #[test]
    fn combining_marks_are_individual_scalar_positions() {
        let mut prompt = SearchPrompt::with_query("e\u{301}x");

        prompt.handle_key(&key(KeyCode::Home));
        prompt.handle_key(&key(KeyCode::Right));
        assert_eq!(prompt.cursor(), 1);
        prompt.handle_key(&key(KeyCode::Right));
        assert_eq!(prompt.cursor(), "e\u{301}".len());
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Backspace)),
            SearchPromptAction::QueryChanged
        );

        assert_eq!(prompt.query(), "ex");
        assert_eq!(prompt.cursor(), 1);
        assert_cursor_is_valid(&prompt);
    }

    #[test]
    fn walks_emoji_sequences_without_breaking_utf8() {
        let mut prompt = SearchPrompt::with_query("👩\u{200d}💻!");

        for _ in 0..4 {
            prompt.handle_key(&key(KeyCode::Left));
            assert_cursor_is_valid(&prompt);
        }
        assert_eq!(prompt.cursor(), 0);

        for _ in 0..4 {
            prompt.handle_key(&key(KeyCode::Right));
            assert_cursor_is_valid(&prompt);
        }
        assert_eq!(prompt.cursor(), prompt.query().len());
    }

    #[test]
    fn home_end_and_boundary_edits_report_no_op() {
        let mut prompt = SearchPrompt::with_query("abc");

        assert_eq!(
            prompt.handle_key(&key(KeyCode::End)),
            SearchPromptAction::Ignored
        );
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Delete)),
            SearchPromptAction::Ignored
        );
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Home)),
            SearchPromptAction::CursorMoved
        );
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Home)),
            SearchPromptAction::Ignored
        );
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Backspace)),
            SearchPromptAction::Ignored
        );
    }

    #[test]
    fn reports_navigation_and_cancel_commands_without_mutating_query() {
        let mut prompt = SearchPrompt::with_query("needle");

        assert_eq!(
            prompt.handle_key(&key(KeyCode::Enter)),
            SearchPromptAction::NextMatch
        );
        assert_eq!(
            prompt.handle_key(&modified_key(KeyCode::Enter, KeyModifiers::SHIFT)),
            SearchPromptAction::PreviousMatch
        );
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Down)),
            SearchPromptAction::NextMatch
        );
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Up)),
            SearchPromptAction::PreviousMatch
        );
        assert_eq!(
            prompt.handle_key(&key(KeyCode::Esc)),
            SearchPromptAction::Cancel
        );
        assert_eq!(prompt.query(), "needle");
        assert_eq!(prompt.cursor(), "needle".len());
    }

    #[test]
    fn ignores_release_events_and_modified_shortcuts() {
        let mut prompt = SearchPrompt::new();
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );

        assert_eq!(prompt.handle_key(&release), SearchPromptAction::Ignored);
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
                SearchPromptAction::Ignored
            );
        }
        assert_eq!(
            prompt.handle_key(&modified_key(KeyCode::Enter, KeyModifiers::CONTROL)),
            SearchPromptAction::Ignored
        );
        assert_eq!(prompt.query(), "");
    }

    #[test]
    fn shift_does_not_block_printable_characters() {
        let mut prompt = SearchPrompt::new();

        assert_eq!(
            prompt.handle_key(&modified_key(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            SearchPromptAction::QueryChanged
        );
        assert_eq!(prompt.query(), "A");
        assert_eq!(prompt.cursor(), 1);
    }

    #[test]
    fn accepts_repeat_events_for_held_editing_keys() {
        let mut prompt = SearchPrompt::with_query("ab");
        let repeat =
            KeyEvent::new_with_kind(KeyCode::Backspace, KeyModifiers::NONE, KeyEventKind::Repeat);

        assert_eq!(prompt.handle_key(&repeat), SearchPromptAction::QueryChanged);
        assert_eq!(prompt.query(), "a");
    }

    #[test]
    fn paste_inserts_only_the_first_line_at_the_cursor() {
        let mut prompt = SearchPrompt::with_query("ab");
        prompt.handle_key(&key(KeyCode::Left));

        assert_eq!(
            prompt.handle_paste("日本🦀\r\nignored"),
            SearchPromptAction::QueryChanged
        );
        assert_eq!(prompt.query(), "a日本🦀b");
        assert_eq!(prompt.cursor(), "a日本🦀".len());
        assert_cursor_is_valid(&prompt);
    }

    #[test]
    fn empty_first_paste_line_is_ignored() {
        let mut prompt = SearchPrompt::with_query("unchanged");

        assert_eq!(
            prompt.handle_paste("\nsecond line"),
            SearchPromptAction::Ignored
        );
        assert_eq!(prompt.query(), "unchanged");
        assert_eq!(prompt.cursor(), "unchanged".len());
    }

    #[test]
    fn paste_preserves_combining_characters_and_tabs() {
        let mut prompt = SearchPrompt::new();

        assert_eq!(
            prompt.handle_paste("e\u{301}\t🦀"),
            SearchPromptAction::QueryChanged
        );
        assert_eq!(prompt.query(), "e\u{301}\t🦀");
        assert_eq!(prompt.cursor(), prompt.query().len());
        assert_cursor_is_valid(&prompt);
    }
}
