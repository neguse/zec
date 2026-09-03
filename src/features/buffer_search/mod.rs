//! Find, replace, and go to line on the active Editor, through Zed's
//! `SearchableItem`: matching, highlights, match activation, and the
//! replacement transactions are Zed's. zec owns only the prompts and the
//! index of the active match.
//!
//! A prompt owns input while it is open, so the buffer cannot change under
//! the matches except through a replacement, after which the search runs
//! again.

use std::{ops::Range, sync::Arc};

use anyhow::Result;
use editor::Editor;
use gpui::{AsyncApp, WindowHandle};
use multi_buffer::Anchor;
use project::search::SearchQuery;
use util::paths::PathMatcher;
use workspace::searchable::{Direction, SearchToken, SearchableItem as _};

use crate::{
    app::{
        event::Event,
        feature::{Ctx, FeatureEvent},
        overlay::{Overlay, PromptTarget},
    },
    terminal::prompt::LinePrompt,
    zed,
};

const FIND: &str = "Find";
const REPLACE: &str = "Replace with";
const GO_TO_LINE: &str = "Go to line";

#[derive(Debug)]
pub enum BufferSearchEvent {
    /// The matches of the search of `generation`; stale generations are
    /// dropped by [`BufferSearch::update`].
    Matches {
        generation: u64,
        matches: Vec<Range<Anchor>>,
    },
}

#[derive(Default)]
pub struct BufferSearch {
    generation: u64,
    /// The text searched for; the matches below belong to it.
    text: String,
    matches: Vec<Range<Anchor>>,
    active: Option<usize>,
    /// The editor holding the match highlights, so they are cleared even
    /// when the active tab changed in between.
    highlighted: Option<WindowHandle<Editor>>,
    /// How many matches the last replacement touched, reported with the
    /// count that follows it.
    replaced: Option<usize>,
}

impl BufferSearch {
    /// `Find`: the prompt starts from the selection, through Zed's query
    /// suggestion, and searches as it changes. Zed also seeds the word at
    /// the caret, selected in its search field so typing replaces it; the
    /// line prompt has no selection, so an empty selection starts empty.
    pub fn find(&mut self, ctx: &mut Ctx, cx: &mut AsyncApp) {
        self.clear_highlights(cx);
        let seed = ctx
            .editor
            .update(cx, |editor, window, cx| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let selection = editor.selections.newest_anchor();
                if selection.start.cmp(&selection.end, &snapshot).is_eq() {
                    String::new()
                } else {
                    editor.query_suggestion(None, window, cx)
                }
            })
            .unwrap_or_default();
        ctx.overlays.clear();
        ctx.overlays.push(Overlay::Prompt {
            label: FIND,
            line: LinePrompt::with_text(&seed),
            target: PromptTarget::Find,
            feedback: None,
        });
        self.query_changed(ctx, &seed, cx);
    }

    /// `Replace`: needs something to search for; without it the Find prompt
    /// opens instead.
    pub fn replace(&mut self, ctx: &mut Ctx, cx: &mut AsyncApp) {
        if self.text.is_empty() {
            self.find(ctx, cx);
            ctx.overlays.set_feedback("search first, then replace");
            return;
        }
        let stale = self.highlighted != Some(ctx.editor) || self.matches.is_empty();
        ctx.overlays.clear();
        ctx.overlays.push(Overlay::Prompt {
            label: REPLACE,
            line: LinePrompt::new(),
            target: PromptTarget::Replace,
            feedback: Some(self.feedback()),
        });
        if stale {
            self.clear_highlights(cx);
            self.search(ctx, cx);
        }
    }

    pub fn go_to_line(&mut self, ctx: &mut Ctx) {
        ctx.overlays.clear();
        ctx.overlays.push(Overlay::Prompt {
            label: GO_TO_LINE,
            line: LinePrompt::new(),
            target: PromptTarget::GoToLine,
            feedback: None,
        });
    }

    /// The Find prompt's text changed.
    pub fn query_changed(&mut self, ctx: &mut Ctx, text: &str, cx: &mut AsyncApp) {
        self.text = text.to_owned();
        self.matches.clear();
        self.active = None;
        self.generation += 1;
        if text.is_empty() {
            self.clear_highlights(cx);
            return;
        }
        self.search(ctx, cx);
    }

    fn search(&mut self, ctx: &mut Ctx, cx: &mut AsyncApp) {
        let query = match self.query(None) {
            Ok(query) => Arc::new(query),
            Err(error) => {
                ctx.overlays
                    .set_feedback(format!("invalid search: {error:#}"));
                return;
            }
        };
        let generation = self.generation;
        let Ok(task) = ctx.editor.update(cx, |editor, window, cx| {
            editor.find_matches(query, window, cx)
        }) else {
            return;
        };
        let events = ctx.events.clone();
        cx.spawn(async move |_| {
            let matches = task.await;
            let _ = events
                .send(Event::Feature(FeatureEvent::BufferSearch(
                    BufferSearchEvent::Matches {
                        generation,
                        matches,
                    },
                )))
                .await;
        })
        .detach();
    }

    /// Matches arrived: Zed highlights them and moves to the one at or
    /// after the caret, as its search bar does while typing.
    pub fn update(&mut self, ctx: &mut Ctx, event: BufferSearchEvent, cx: &mut AsyncApp) {
        let BufferSearchEvent::Matches {
            generation,
            matches,
        } = event;
        if generation != self.generation {
            return;
        }
        if !matches!(
            ctx.overlays.top(),
            Some(Overlay::Prompt {
                target: PromptTarget::Find | PromptTarget::Replace,
                ..
            })
        ) {
            return;
        }
        self.matches = matches;
        let matches = &self.matches;
        self.active = ctx
            .editor
            .update(cx, |editor, window, cx| {
                let token = SearchToken::default();
                let active = if matches.is_empty() {
                    None
                } else {
                    editor.active_match_index(Direction::Next, matches, token, window, cx)
                };
                editor.update_matches(matches, active, token, window, cx);
                if let Some(index) = active {
                    editor.activate_match(index, matches, token, window, cx);
                }
                active
            })
            .unwrap_or(None);
        self.highlighted = Some(ctx.editor);
        ctx.overlays.set_feedback(self.feedback());
    }

    /// Enter and Shift-Enter in the Find prompt.
    pub fn step(&mut self, ctx: &mut Ctx, direction: Direction, cx: &mut AsyncApp) {
        if self.matches.is_empty() {
            ctx.overlays.set_feedback(self.feedback());
            return;
        }
        let current = self.active.unwrap_or(0);
        let matches = &self.matches;
        let next = ctx.editor.update(cx, |editor, window, cx| {
            let token = SearchToken::default();
            let index =
                editor.match_index_for_direction(matches, current, direction, 1, token, window, cx);
            editor.update_matches(matches, Some(index), token, window, cx);
            editor.activate_match(index, matches, token, window, cx);
            index
        });
        if let Ok(index) = next {
            self.active = Some(index);
        }
        ctx.overlays.set_feedback(self.feedback());
    }

    /// Enter in the Replace prompt: the active match, then the search runs
    /// again so the next one becomes active.
    pub fn replace_current(&mut self, ctx: &mut Ctx, replacement: &str, cx: &mut AsyncApp) {
        let Some(matched) = self
            .active
            .and_then(|index| self.matches.get(index))
            .cloned()
        else {
            ctx.overlays.set_feedback("no match to replace");
            return;
        };
        let query = match self.query(Some(replacement)) {
            Ok(query) => query,
            Err(error) => {
                ctx.overlays
                    .set_feedback(format!("invalid search: {error:#}"));
                return;
            }
        };
        let _ = ctx.editor.update(cx, |editor, window, cx| {
            editor.replace(&matched, &query, SearchToken::default(), window, cx);
        });
        self.replaced = Some(1);
        self.generation += 1;
        self.search(ctx, cx);
    }

    /// Shift-Enter in the Replace prompt: every match in one transaction.
    pub fn replace_all(&mut self, ctx: &mut Ctx, replacement: &str, cx: &mut AsyncApp) {
        if self.matches.is_empty() {
            ctx.overlays.set_feedback("no matches to replace");
            return;
        }
        let query = match self.query(Some(replacement)) {
            Ok(query) => query,
            Err(error) => {
                ctx.overlays
                    .set_feedback(format!("invalid search: {error:#}"));
                return;
            }
        };
        let matches = &self.matches;
        let _ = ctx.editor.update(cx, |editor, window, cx| {
            editor.replace_all(
                &mut matches.iter(),
                &query,
                SearchToken::default(),
                window,
                cx,
            );
        });
        self.replaced = Some(self.matches.len());
        self.generation += 1;
        self.search(ctx, cx);
    }

    /// Esc: as in Zed, the selection stays on the last match and the
    /// highlights go away.
    pub fn cancel(&mut self, cx: &mut AsyncApp) {
        self.clear_highlights(cx);
        self.matches.clear();
        self.active = None;
        self.generation += 1;
    }

    /// Enter in the Go to line prompt: `line` or `line:column`, 1-based.
    pub fn go_to(&mut self, ctx: &mut Ctx, text: &str, cx: &mut AsyncApp) {
        let Some((row, column)) = parse_line_column(text) else {
            ctx.overlays.set_feedback("enter a line, or line:column");
            return;
        };
        if let Err(error) =
            zed::editor::place_caret_at_point(&ctx.editor, text::Point::new(row, column), cx)
        {
            ctx.overlays
                .set_feedback(format!("go to line failed: {error:#}"));
            return;
        }
        ctx.overlays.pop();
        ctx.status.set(format!("line {}", row + 1));
    }

    fn query(&self, replacement: Option<&str>) -> Result<SearchQuery> {
        let query = SearchQuery::text(
            &self.text,
            false,
            false,
            false,
            PathMatcher::default(),
            PathMatcher::default(),
            false,
            None,
        )?;
        Ok(match replacement {
            Some(replacement) => query.with_replacement(replacement.to_owned()),
            None => query,
        })
    }

    fn clear_highlights(&mut self, cx: &mut AsyncApp) {
        if let Some(editor) = self.highlighted.take() {
            let _ = editor.update(cx, |editor, window, cx| editor.clear_matches(window, cx));
        }
    }

    fn feedback(&mut self) -> String {
        let count = match (self.active, self.matches.len()) {
            (_, 0) => "no matches".to_owned(),
            (Some(index), total) => format!("{} of {total}", index + 1),
            (None, total) => format!("{total} matches"),
        };
        match self.replaced.take() {
            Some(replaced) => format!("replaced {replaced}; {count}"),
            None => count,
        }
    }
}

/// `line` or `line:column`, both 1-based, to a 0-based point. The column
/// is a byte offset clipped by the editor, as Zed's go to line treats it.
fn parse_line_column(text: &str) -> Option<(u32, u32)> {
    let text = text.trim();
    let (line, column) = match text.split_once(':') {
        Some((line, column)) => (line.trim(), column.trim().parse::<u32>().ok()?),
        None => (text, 1),
    };
    let line = line.parse::<u32>().ok()?;
    if line == 0 || column == 0 {
        return None;
    }
    Some((line - 1, column - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_and_column_are_one_based() {
        assert_eq!(parse_line_column("1"), Some((0, 0)));
        assert_eq!(parse_line_column(" 12 : 4 "), Some((11, 3)));
        assert_eq!(parse_line_column("0"), None);
        assert_eq!(parse_line_column("3:0"), None);
        assert_eq!(parse_line_column("x"), None);
        assert_eq!(parse_line_column(""), None);
    }
}
