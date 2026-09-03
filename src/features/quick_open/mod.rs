//! Quick Open: a picker over the files of the visible worktree, matched by
//! the same fuzzy matcher Zed's file finder uses.
//!
//! The worktree snapshot is Zed's file set, ignore rules included. zec
//! keeps no index: every query is a fresh match over the current snapshot,
//! and the first query waits for the initial scan so its results are
//! complete rather than fast.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use fuzzy_nucleo::PathMatchCandidateSet as _;
use gpui::AsyncApp;
use project::{Candidates, PathMatchCandidateSet};

use crate::{
    app::{
        event::Event,
        feature::{Ctx, FeatureEvent},
        overlay::{Overlay, PickerOwner, PickerPayload},
    },
    terminal::{
        picker::{PickerEntry, PickerList},
        prompt::LinePrompt,
    },
};

const TITLE: &str = "Quick open";
const MAX_RESULTS: usize = 100;

#[derive(Debug)]
pub enum QuickOpenEvent {
    /// The matches for the query of `generation`; stale generations are
    /// dropped by [`QuickOpen::update`].
    Matches {
        generation: u64,
        entries: Vec<PickerEntry<PickerPayload>>,
    },
}

#[derive(Default)]
pub struct QuickOpen {
    generation: u64,
    cancel: Arc<AtomicBool>,
}

impl QuickOpen {
    pub fn open(&mut self, ctx: &mut Ctx, cx: &mut AsyncApp) {
        if ctx.root.is_none() {
            ctx.status.set("quick open needs a directory root");
            return;
        }
        ctx.overlays.clear();
        ctx.overlays.push(Overlay::Picker {
            title: TITLE,
            query: LinePrompt::new(),
            list: PickerList::new(Vec::new()),
            owner: PickerOwner::QuickOpen,
        });
        self.search(ctx, "", cx);
    }

    pub fn query_changed(&mut self, ctx: &mut Ctx, query: &str, cx: &mut AsyncApp) {
        self.search(ctx, query, cx);
    }

    pub fn update(&mut self, ctx: &mut Ctx, event: QuickOpenEvent) {
        let QuickOpenEvent::Matches {
            generation,
            entries,
        } = event;
        if generation != self.generation {
            return;
        }
        if let Some(Overlay::Picker {
            list,
            owner: PickerOwner::QuickOpen,
            ..
        }) = ctx.overlays.top_mut()
        {
            list.replace(entries);
        }
    }

    fn search(&mut self, ctx: &mut Ctx, query: &str, cx: &mut AsyncApp) {
        self.cancel.store(true, Ordering::Release);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.generation += 1;
        let generation = self.generation;
        let cancel = self.cancel.clone();
        let worktree_store = ctx.services.worktree_store.clone();
        let events = ctx.events.clone();
        let query = query.to_owned();
        cx.spawn(async move |cx| {
            let worktrees = worktree_store.read_with(cx, |store, cx| {
                store.visible_worktrees(cx).collect::<Vec<_>>()
            });
            for worktree in &worktrees {
                let scan = worktree.read_with(cx, |worktree, _| {
                    worktree.as_local().map(|local| local.scan_complete())
                });
                if let Some(scan) = scan {
                    scan.await;
                }
            }
            if cancel.load(Ordering::Acquire) {
                return;
            }
            let sets = cx.update(|cx| {
                worktrees
                    .iter()
                    .map(|worktree| PathMatchCandidateSet {
                        snapshot: worktree.read(cx).snapshot(),
                        include_ignored: false,
                        include_root_name: false,
                        candidates: Candidates::Files,
                    })
                    .collect::<Vec<_>>()
            });
            let entries = if query.trim().is_empty() {
                sets.iter()
                    .flat_map(|set| {
                        set.candidates(0).map(|candidate| {
                            entry(set, candidate.path.display(set.path_style()).into_owned())
                        })
                    })
                    .take(MAX_RESULTS)
                    .collect()
            } else {
                let matches = fuzzy_nucleo::match_path_sets(
                    sets.as_slice(),
                    &query,
                    &None,
                    fuzzy_nucleo::Case::Ignore,
                    MAX_RESULTS,
                    &cancel,
                    cx.background_executor().clone(),
                )
                .await;
                matches
                    .into_iter()
                    .filter_map(|found| {
                        let set = sets.iter().find(|set| set.id() == found.worktree_id)?;
                        Some(entry(
                            set,
                            found.path.display(set.path_style()).into_owned(),
                        ))
                    })
                    .collect()
            };
            if cancel.load(Ordering::Acquire) {
                return;
            }
            let _ = events
                .send(Event::Feature(FeatureEvent::QuickOpen(
                    QuickOpenEvent::Matches {
                        generation,
                        entries,
                    },
                )))
                .await;
        })
        .detach();
    }
}

fn entry(set: &PathMatchCandidateSet, relative: String) -> PickerEntry<PickerPayload> {
    let absolute: PathBuf = set.snapshot.abs_path().join(&relative);
    PickerEntry {
        label: relative,
        detail: String::new(),
        enabled: true,
        payload: PickerPayload::Path(absolute),
    }
}
