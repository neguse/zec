//! Project search: Zed's `Project::search` over the visible worktree, with
//! the hits listed in a picker and opened at their location.
//!
//! Zed owns the file set, the ignore rules, the matcher, and the anchors.
//! zec projects each hit to `path:line  preview`, orders hits by path, and
//! stops collecting at a bound so a common word cannot flood the picker.
//! Regex, match case, and whole word are toggled in the prompt, shown after
//! its text, and kept for the rest of the session.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::Result;
use gpui::AsyncApp;
use project::search::{SearchQuery, SearchResult};
use text::{Point, ToPoint as _};
use util::paths::PathMatcher;

use crate::{
    app::{
        event::Event,
        feature::{Ctx, FeatureEvent},
        overlay::{Overlay, PickerOwner, PickerPayload, PromptTarget},
    },
    terminal::{
        picker::{PickerEntry, PickerList},
        prompt::LinePrompt,
    },
    zed::services::buffer_state,
};

const PROMPT: &str = "Project search";
const TITLE: &str = "Matches";
const MAX_HITS: usize = 1000;
const PREVIEW_CHARS: usize = 120;

#[derive(Debug)]
pub enum ProjectSearchEvent {
    /// Every hit of the search of `generation`, ordered by path and
    /// position; stale generations are dropped by [`ProjectSearch::update`].
    Results {
        generation: u64,
        query: String,
        entries: Vec<PickerEntry<PickerPayload>>,
        truncated: bool,
    },
}

/// The toggles a search runs with; they persist for the session.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SearchOptions {
    pub regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchOption {
    Regex,
    CaseSensitive,
    WholeWord,
}

impl SearchOptions {
    fn toggle(&mut self, option: SearchOption) {
        let slot = match option {
            SearchOption::Regex => &mut self.regex,
            SearchOption::CaseSensitive => &mut self.case_sensitive,
            SearchOption::WholeWord => &mut self.whole_word,
        };
        *slot = !*slot;
    }

    /// The active options as the prompt shows them after its text.
    fn describe(self) -> Option<String> {
        let names = [
            (self.regex, "regex"),
            (self.case_sensitive, "match case"),
            (self.whole_word, "whole word"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect::<Vec<_>>();
        (!names.is_empty()).then(|| names.join(", "))
    }
}

#[derive(Default)]
pub struct ProjectSearch {
    generation: u64,
    cancel: Arc<AtomicBool>,
    options: SearchOptions,
    /// The text of the last search, rerun when an option changes while
    /// its hits are listed.
    last_query: Option<String>,
}

impl ProjectSearch {
    pub fn open(&mut self, ctx: &mut Ctx) {
        if ctx.root.is_none() {
            ctx.status.set("project search needs a directory root");
            return;
        }
        ctx.overlays.clear();
        ctx.overlays.push(Overlay::Prompt {
            label: PROMPT,
            line: LinePrompt::new(),
            target: PromptTarget::ProjectSearch,
            feedback: self.options.describe(),
        });
    }

    /// The prompt's text changed, which cleared its feedback; the active
    /// options return after the new text.
    pub fn query_changed(&mut self, ctx: &mut Ctx) {
        self.show_options(ctx);
    }

    /// Flips one option: in the prompt it shows after the text, over the
    /// hits it reruns the search, elsewhere the status row reports it.
    pub fn toggle(&mut self, ctx: &mut Ctx, option: SearchOption, cx: &mut AsyncApp) {
        self.options.toggle(option);
        match ctx.overlays.top() {
            Some(Overlay::Prompt {
                target: PromptTarget::ProjectSearch,
                ..
            }) => self.show_options(ctx),
            Some(Overlay::Picker {
                owner: PickerOwner::ProjectSearch,
                ..
            }) => {
                if let Some(text) = self.last_query.clone() {
                    self.submit(ctx, &text, cx);
                }
            }
            _ => ctx.status.set(format!(
                "project search options: {}",
                self.options.describe().unwrap_or_else(|| "none".to_owned())
            )),
        }
    }

    fn show_options(&self, ctx: &mut Ctx) {
        if let Some(Overlay::Prompt {
            target: PromptTarget::ProjectSearch,
            feedback,
            ..
        }) = ctx.overlays.top_mut()
        {
            *feedback = self.options.describe();
        }
    }

    /// The prompt was submitted: a picker replaces it and fills once Zed
    /// has reported every matching buffer.
    pub fn submit(&mut self, ctx: &mut Ctx, text: &str, cx: &mut AsyncApp) {
        let Some(root) = ctx.root.map(Path::to_path_buf) else {
            return;
        };
        if text.trim().is_empty() {
            return;
        }
        let query = match build_query(text, self.options) {
            Ok(query) => query,
            Err(error) => {
                // An invalid pattern keeps the prompt open to be fixed.
                let message = format!("invalid pattern: {error:#}");
                match ctx.overlays.top() {
                    Some(Overlay::Prompt { .. }) => ctx.overlays.set_feedback(message),
                    _ => ctx.status.set(message),
                }
                return;
            }
        };
        self.last_query = Some(text.to_owned());
        ctx.overlays.clear();
        ctx.overlays.push(Overlay::Picker {
            title: TITLE,
            query: LinePrompt::new(),
            list: PickerList::new(Vec::new()),
            owner: PickerOwner::ProjectSearch,
        });

        self.cancel.store(true, Ordering::Release);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.generation += 1;
        let generation = self.generation;
        let cancel = self.cancel.clone();
        let project = ctx.services.project.clone();
        let events = ctx.events.clone();
        let text = text.to_owned();
        cx.spawn(async move |cx| {
            // The results stay alive until the stream is drained; dropping
            // them cancels Zed's search task.
            let results = project.update(cx, |project, cx| project.search(query, cx));
            let mut hits: Vec<(PathBuf, Point, String)> = Vec::new();
            let mut truncated = false;
            while let Ok(result) = results.rx.recv().await {
                if cancel.load(Ordering::Acquire) {
                    return;
                }
                match result {
                    SearchResult::Buffer { buffer, ranges } => {
                        let Some(path) = buffer_state(&buffer, cx).path else {
                            continue;
                        };
                        let lines = buffer.read_with(cx, |buffer, _| {
                            let snapshot = buffer.snapshot();
                            ranges
                                .iter()
                                .map(|range| {
                                    let point = range.start.to_point(&snapshot);
                                    let line = snapshot
                                        .text_for_range(
                                            Point::new(point.row, 0)
                                                ..Point::new(
                                                    point.row,
                                                    snapshot.line_len(point.row),
                                                ),
                                        )
                                        .collect::<String>();
                                    (point, line)
                                })
                                .collect::<Vec<_>>()
                        });
                        for (point, line) in lines {
                            if hits.len() >= MAX_HITS {
                                truncated = true;
                                break;
                            }
                            hits.push((path.clone(), point, line));
                        }
                        if truncated {
                            break;
                        }
                    }
                    SearchResult::LimitReached => {
                        truncated = true;
                        break;
                    }
                    SearchResult::WaitingForScan | SearchResult::Searching => {}
                }
            }
            drop(results);
            if cancel.load(Ordering::Acquire) {
                return;
            }
            hits.sort_by(|(left_path, left, _), (right_path, right, _)| {
                left_path.cmp(right_path).then(left.cmp(right))
            });
            let entries = hits
                .into_iter()
                .map(|(path, point, line)| PickerEntry {
                    label: format!(
                        "{}:{}",
                        path.strip_prefix(&root).unwrap_or(&path).display(),
                        point.row + 1
                    ),
                    detail: preview(&line),
                    enabled: true,
                    payload: PickerPayload::Location {
                        path,
                        row: point.row,
                        column: point.column,
                    },
                })
                .collect();
            let _ = events
                .send(Event::Feature(FeatureEvent::ProjectSearch(
                    ProjectSearchEvent::Results {
                        generation,
                        query: text,
                        entries,
                        truncated,
                    },
                )))
                .await;
        })
        .detach();
    }

    pub fn update(&mut self, ctx: &mut Ctx, event: ProjectSearchEvent) {
        let ProjectSearchEvent::Results {
            generation,
            query,
            mut entries,
            truncated,
        } = event;
        if generation != self.generation {
            return;
        }
        let Some(Overlay::Picker {
            list,
            owner: PickerOwner::ProjectSearch,
            ..
        }) = ctx.overlays.top_mut()
        else {
            return;
        };
        if entries.is_empty() {
            ctx.overlays.pop();
            ctx.status.set(format!("no matches for {query}"));
            return;
        }
        if truncated {
            entries.push(PickerEntry {
                label: format!("more than {MAX_HITS} matches; refine the search"),
                detail: String::new(),
                enabled: false,
                payload: PickerPayload::Location {
                    path: PathBuf::new(),
                    row: 0,
                    column: 0,
                },
            });
        }
        list.replace(entries);
    }
}

/// Zed's query for `text` under `options`; a malformed regex is an error.
fn build_query(text: &str, options: SearchOptions) -> Result<SearchQuery> {
    if options.regex {
        SearchQuery::regex(
            text,
            options.whole_word,
            options.case_sensitive,
            false,
            false,
            PathMatcher::default(),
            PathMatcher::default(),
            false,
            None,
        )
    } else {
        SearchQuery::text(
            text,
            options.whole_word,
            options.case_sensitive,
            false,
            PathMatcher::default(),
            PathMatcher::default(),
            false,
            None,
        )
    }
}

/// One line of context, trimmed and bounded so a minified file cannot
/// take the whole row.
fn preview(line: &str) -> String {
    let trimmed = line.trim();
    let mut preview = trimmed.chars().take(PREVIEW_CHARS).collect::<String>();
    if preview.len() < trimmed.len() {
        preview.push('…');
    }
    preview
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_describe_only_the_active_ones() {
        let mut options = SearchOptions::default();
        assert_eq!(options.describe(), None);
        options.toggle(SearchOption::Regex);
        options.toggle(SearchOption::WholeWord);
        assert_eq!(options.describe().as_deref(), Some("regex, whole word"));
        options.toggle(SearchOption::Regex);
        assert_eq!(options.describe().as_deref(), Some("whole word"));
    }

    #[test]
    fn queries_carry_the_options() {
        let literal = build_query("a.b", SearchOptions::default()).unwrap();
        assert!(!literal.is_regex());
        assert!(!literal.case_sensitive());
        assert!(!literal.whole_word());

        let options = SearchOptions {
            regex: true,
            case_sensitive: true,
            whole_word: true,
        };
        let regex = build_query("a.b", options).unwrap();
        assert!(regex.is_regex());
        assert!(regex.case_sensitive());
        assert!(regex.whole_word());

        assert!(build_query("(", options).is_err());
    }

    #[test]
    fn previews_are_trimmed_and_bounded() {
        assert_eq!(preview("  fn main() {}  \n"), "fn main() {}");
        let long = "x".repeat(PREVIEW_CHARS + 5);
        let preview = preview(&long);
        assert_eq!(preview.chars().count(), PREVIEW_CHARS + 1);
        assert!(preview.ends_with('…'));
    }
}
