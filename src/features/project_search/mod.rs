//! Project search: Zed's `Project::search` over the visible worktree, with
//! the hits listed in a picker and opened at their location.
//!
//! Zed owns the file set, the ignore rules, the matcher, and the anchors.
//! zec projects each hit to `path:line  preview`, orders hits by path, and
//! stops collecting at a bound so a common word cannot flood the picker.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

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

#[derive(Default)]
pub struct ProjectSearch {
    generation: u64,
    cancel: Arc<AtomicBool>,
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
            feedback: None,
        });
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
        let query = match SearchQuery::text(
            text,
            false,
            false,
            false,
            PathMatcher::default(),
            PathMatcher::default(),
            false,
            None,
        ) {
            Ok(query) => query,
            Err(error) => {
                ctx.overlays.clear();
                ctx.status.set(format!("search failed: {error:#}"));
                return;
            }
        };
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
    fn previews_are_trimmed_and_bounded() {
        assert_eq!(preview("  fn main() {}  \n"), "fn main() {}");
        let long = "x".repeat(PREVIEW_CHARS + 5);
        let preview = preview(&long);
        assert_eq!(preview.chars().count(), PREVIEW_CHARS + 1);
        assert!(preview.ends_with('…'));
    }
}
