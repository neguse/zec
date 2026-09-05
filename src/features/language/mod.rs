//! Language intelligence: completion, hover, diagnostics, locations, rename,
//! and code actions, all served by the language servers Zed starts for the
//! Project. zec sends Zed's requests for the caret position and projects
//! the answers into pickers, prompts, and a read-only text overlay; the
//! edits a completion, rename, or code action makes are Zed transactions.
//!
//! Every request is tagged with a generation and the tab it was made for;
//! an answer that arrives after another request or another tab is dropped.

use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use gpui::{AsyncApp, Entity};
use language::Buffer;
use project::{CodeAction, Completion, CompletionSource, PrepareRenameResponse};
use text::{Point, ToPoint as _};

use crate::{
    app::{
        event::Event,
        feature::{Ctx, FeatureEvent},
        overlay::{Overlay, PickerOwner, PickerPayload, PromptTarget},
        workspace::ItemId,
    },
    terminal::{
        picker::{PickerEntry, PickerList},
        prompt::LinePrompt,
    },
    zed::{self, services::buffer_state},
};

const COMPLETIONS: &str = "Completions";
const HOVER: &str = "Hover";
const DIAGNOSTICS: &str = "Diagnostics";
const CODE_ACTIONS: &str = "Code actions";
const RENAME: &str = "Rename";
const MAX_ROWS: usize = 500;

/// Which location request a picker shows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocationKind {
    Definition,
    TypeDefinition,
    References,
}

impl LocationKind {
    fn title(self) -> &'static str {
        match self {
            Self::Definition => "Definitions",
            Self::TypeDefinition => "Type definitions",
            Self::References => "References",
        }
    }

    fn none(self) -> &'static str {
        match self {
            Self::Definition => "no definition",
            Self::TypeDefinition => "no type definition",
            Self::References => "no references",
        }
    }
}

/// A place in a file, ready to open.
#[derive(Debug)]
pub struct Hit {
    pub path: PathBuf,
    pub row: u32,
    pub column: u32,
}

pub enum LanguageEvent {
    Completions {
        generation: u64,
        query: String,
        completions: Vec<Completion>,
    },
    Hover {
        generation: u64,
        lines: Vec<String>,
    },
    Locations {
        generation: u64,
        kind: LocationKind,
        hits: Vec<(Hit, String)>,
    },
    Diagnostics {
        generation: u64,
        entries: Vec<PickerEntry<PickerPayload>>,
    },
    RenamePrepared {
        generation: u64,
        point: Point,
        current: Option<String>,
    },
    CodeActions {
        generation: u64,
        actions: Vec<CodeAction>,
    },
    /// A rename or code action finished; the message is ready to show.
    Applied {
        generation: u64,
        result: Result<String, String>,
    },
}

impl std::fmt::Debug for LanguageEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Completions { .. } => "Completions",
            Self::Hover { .. } => "Hover",
            Self::Locations { .. } => "Locations",
            Self::Diagnostics { .. } => "Diagnostics",
            Self::RenamePrepared { .. } => "RenamePrepared",
            Self::CodeActions { .. } => "CodeActions",
            Self::Applied { .. } => "Applied",
        };
        f.write_str(name)
    }
}

/// What the app does after a language event.
pub enum LanguageOutcome {
    Consumed,
    /// Open the file at the hit in the active pane.
    Open(Hit),
}

#[derive(Default)]
pub struct Language {
    generation: u64,
    /// The tab the pending request was made for.
    item: Option<ItemId>,
    cancel: Arc<AtomicBool>,
    /// The completions of the open picker, indexed by its entries.
    completions: Rc<RefCell<Box<[Completion]>>>,
    /// The code actions of the open picker, indexed by its entries.
    actions: Vec<CodeAction>,
    /// Where the pending rename applies.
    rename_point: Option<Point>,
}

impl Language {
    /// `ShowCompletions`: the items Zed's servers offer at the caret, in a
    /// picker whose query starts as the word before the caret.
    pub fn show_completions(&mut self, ctx: &mut Ctx, cx: &mut AsyncApp) {
        let Some((buffer, point)) = self.begin(ctx, cx) else {
            return;
        };
        let generation = self.generation;
        let cancel = self.cancel.clone();
        let project = ctx.services.project.clone();
        let events = ctx.events.clone();
        cx.spawn(async move |cx| {
            let responses = project
                .update(cx, |project, cx| {
                    project.completions(
                        &buffer,
                        point,
                        lsp::CompletionContext {
                            trigger_kind: lsp::CompletionTriggerKind::INVOKED,
                            trigger_character: None,
                        },
                        cx,
                    )
                })
                .await;
            if cancel.load(Ordering::Acquire) {
                return;
            }
            let completions = match responses {
                Ok(responses) => responses
                    .into_iter()
                    .flat_map(|response| response.completions)
                    .collect::<Vec<_>>(),
                Err(error) => {
                    log::warn!("completion request failed: {error:#}");
                    Vec::new()
                }
            };
            // The word being completed is the query Zed would filter by.
            let query = completions
                .first()
                .map(|completion| {
                    buffer.read_with(cx, |buffer, _| {
                        let snapshot = buffer.snapshot();
                        let start = completion.replace_range.start.to_point(&snapshot);
                        snapshot
                            .text_for_range(start..point.max(start))
                            .collect::<String>()
                    })
                })
                .unwrap_or_default();
            let _ = events
                .send(Event::Feature(FeatureEvent::Language(
                    LanguageEvent::Completions {
                        generation,
                        query,
                        completions,
                    },
                )))
                .await;
        })
        .detach();
    }

    /// `Hover`: the servers' documentation for the symbol at the caret.
    pub fn hover(&mut self, ctx: &mut Ctx, cx: &mut AsyncApp) {
        let Some((buffer, point)) = self.begin(ctx, cx) else {
            return;
        };
        let generation = self.generation;
        let cancel = self.cancel.clone();
        let project = ctx.services.project.clone();
        let events = ctx.events.clone();
        cx.spawn(async move |cx| {
            let hovers = project
                .update(cx, |project, cx| project.hover(&buffer, point, cx))
                .await
                .unwrap_or_default();
            if cancel.load(Ordering::Acquire) {
                return;
            }
            let lines = hovers
                .iter()
                .flat_map(|hover| hover.contents.iter())
                .flat_map(|block| block.text.lines().map(str::to_owned).collect::<Vec<_>>())
                .filter(|line| !line.trim_start().starts_with("```"))
                .take(MAX_ROWS)
                .collect();
            let _ = events
                .send(Event::Feature(FeatureEvent::Language(
                    LanguageEvent::Hover { generation, lines },
                )))
                .await;
        })
        .detach();
    }

    /// `GoToDefinition`, `GoToTypeDefinition`, `FindReferences`: one hit
    /// opens at once, several are listed.
    pub fn locations(&mut self, ctx: &mut Ctx, kind: LocationKind, cx: &mut AsyncApp) {
        let Some((buffer, point)) = self.begin(ctx, cx) else {
            return;
        };
        let generation = self.generation;
        let cancel = self.cancel.clone();
        let project = ctx.services.project.clone();
        let events = ctx.events.clone();
        let root = ctx.root.map(|root| root.to_path_buf());
        cx.spawn(async move |cx| {
            let targets = |links: Option<Vec<project::LocationLink>>| {
                links
                    .unwrap_or_default()
                    .into_iter()
                    .map(|link| link.target)
                    .collect::<Vec<_>>()
            };
            let locations = match kind {
                LocationKind::Definition => project
                    .update(cx, |project, cx| project.definitions(&buffer, point, cx))
                    .await
                    .map(targets),
                LocationKind::TypeDefinition => project
                    .update(cx, |project, cx| {
                        project.type_definitions(&buffer, point, cx)
                    })
                    .await
                    .map(targets),
                LocationKind::References => project
                    .update(cx, |project, cx| project.references(&buffer, point, cx))
                    .await
                    .map(|locations| locations.unwrap_or_default()),
            };
            if cancel.load(Ordering::Acquire) {
                return;
            }
            let locations = match locations {
                Ok(locations) => locations,
                Err(error) => {
                    log::warn!("{} request failed: {error:#}", kind.title());
                    Vec::new()
                }
            };
            let mut hits = Vec::new();
            for location in locations {
                let Some(path) = buffer_state(&location.buffer, cx).path else {
                    continue;
                };
                let (point, line) = location.buffer.read_with(cx, |buffer, _| {
                    let snapshot = buffer.snapshot();
                    let point = location.range.start.to_point(&snapshot);
                    let line = snapshot
                        .text_for_range(
                            Point::new(point.row, 0)
                                ..Point::new(point.row, snapshot.line_len(point.row)),
                        )
                        .collect::<String>();
                    (point, line)
                });
                let label = format!(
                    "{}:{}  {}",
                    root.as_deref()
                        .and_then(|root| path.strip_prefix(root).ok())
                        .unwrap_or(&path)
                        .display(),
                    point.row + 1,
                    line.trim()
                );
                hits.push((
                    Hit {
                        path,
                        row: point.row,
                        column: point.column,
                    },
                    label,
                ));
            }
            hits.dedup_by(|left, right| left.1 == right.1);
            let _ = events
                .send(Event::Feature(FeatureEvent::Language(
                    LanguageEvent::Locations {
                        generation,
                        kind,
                        hits,
                    },
                )))
                .await;
        })
        .detach();
    }

    /// `Diagnostics`: every diagnostic the servers reported, by file.
    pub fn diagnostics(&mut self, ctx: &mut Ctx, cx: &mut AsyncApp) {
        if ctx.root.is_none() {
            ctx.status.set("diagnostics need a directory root");
            return;
        }
        self.next_generation(ctx);
        let generation = self.generation;
        let cancel = self.cancel.clone();
        let project = ctx.services.project.clone();
        let buffer_store = ctx.services.buffer_store.clone();
        let events = ctx.events.clone();
        let root = ctx.root.map(|root| root.to_path_buf());
        cx.spawn(async move |cx| {
            let mut paths = project.read_with(cx, |project, cx| {
                project
                    .diagnostic_summaries(false, cx)
                    .map(|(path, _, _)| path)
                    .collect::<Vec<_>>()
            });
            paths.sort();
            paths.dedup();
            let mut entries = Vec::new();
            for project_path in paths {
                let buffer = buffer_store
                    .update(cx, |store, cx| store.open_buffer(project_path, cx))
                    .await;
                let Ok(buffer) = buffer else {
                    continue;
                };
                if cancel.load(Ordering::Acquire) {
                    return;
                }
                let Some(path) = buffer_state(&buffer, cx).path else {
                    continue;
                };
                let shown = root
                    .as_deref()
                    .and_then(|root| path.strip_prefix(root).ok())
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                let rows = buffer.read_with(cx, |buffer, _| {
                    let snapshot = buffer.snapshot();
                    snapshot
                        .diagnostics_in_range::<_, Point>(
                            Point::zero()..snapshot.max_point(),
                            false,
                        )
                        .filter(|entry| entry.diagnostic.is_primary)
                        .map(|entry| {
                            (
                                entry.range.start,
                                severity(entry.diagnostic.severity),
                                entry.diagnostic.message.clone(),
                            )
                        })
                        .collect::<Vec<_>>()
                });
                for (point, severity, message) in rows {
                    if entries.len() >= MAX_ROWS {
                        break;
                    }
                    entries.push(PickerEntry {
                        label: format!("{shown}:{}", point.row + 1),
                        detail: format!(
                            "{severity} {}",
                            message.lines().next().unwrap_or_default()
                        ),
                        enabled: true,
                        payload: PickerPayload::Location {
                            path: path.clone(),
                            row: point.row,
                            column: point.column,
                        },
                    });
                }
            }
            let _ = events
                .send(Event::Feature(FeatureEvent::Language(
                    LanguageEvent::Diagnostics {
                        generation,
                        entries,
                    },
                )))
                .await;
        })
        .detach();
    }

    /// `RenameSymbol`: asks the server what can be renamed at the caret,
    /// then prompts for the new name.
    pub fn rename(&mut self, ctx: &mut Ctx, cx: &mut AsyncApp) {
        let Some((buffer, point)) = self.begin(ctx, cx) else {
            return;
        };
        let generation = self.generation;
        let cancel = self.cancel.clone();
        let project = ctx.services.project.clone();
        let events = ctx.events.clone();
        cx.spawn(async move |cx| {
            let prepared = project
                .update(cx, |project, cx| {
                    project.prepare_rename(buffer.clone(), point, cx)
                })
                .await;
            if cancel.load(Ordering::Acquire) {
                return;
            }
            let current = match prepared {
                Ok(PrepareRenameResponse::Success(range)) => {
                    Some(buffer.read_with(cx, |buffer, _| {
                        let snapshot = buffer.snapshot();
                        snapshot.text_for_range(range).collect::<String>()
                    }))
                }
                Ok(PrepareRenameResponse::OnlyUnpreparedRenameSupported) => Some(String::new()),
                Ok(PrepareRenameResponse::InvalidPosition) => None,
                Err(error) => {
                    log::warn!("prepare rename failed: {error:#}");
                    None
                }
            };
            let _ = events
                .send(Event::Feature(FeatureEvent::Language(
                    LanguageEvent::RenamePrepared {
                        generation,
                        point,
                        current,
                    },
                )))
                .await;
        })
        .detach();
    }

    /// The rename prompt was submitted.
    pub fn submit_rename(&mut self, ctx: &mut Ctx, new_name: &str, cx: &mut AsyncApp) {
        let new_name = new_name.trim().to_owned();
        if new_name.is_empty() {
            ctx.overlays.set_feedback("name is empty");
            return;
        }
        let Some(point) = self.rename_point.take() else {
            ctx.overlays.clear();
            return;
        };
        let Some(buffer) = self.active_buffer(ctx) else {
            return;
        };
        ctx.overlays.clear();
        self.next_generation(ctx);
        let generation = self.generation;
        let project = ctx.services.project.clone();
        let events = ctx.events.clone();
        cx.spawn(async move |cx| {
            let result = project
                .update(cx, |project, cx| {
                    project.perform_rename(buffer, point, new_name.clone(), cx)
                })
                .await
                .map(|transaction| {
                    format!("renamed to {new_name} in {} buffer(s)", transaction.0.len())
                })
                .map_err(|error| format!("rename failed: {error:#}"));
            let _ = events
                .send(Event::Feature(FeatureEvent::Language(
                    LanguageEvent::Applied { generation, result },
                )))
                .await;
        })
        .detach();
    }

    /// `CodeActions`: the servers' actions at the caret, in a picker.
    pub fn code_actions(&mut self, ctx: &mut Ctx, cx: &mut AsyncApp) {
        let Some((buffer, point)) = self.begin(ctx, cx) else {
            return;
        };
        let generation = self.generation;
        let cancel = self.cancel.clone();
        let project = ctx.services.project.clone();
        let events = ctx.events.clone();
        cx.spawn(async move |cx| {
            let actions = project
                .update(cx, |project, cx| {
                    project.code_actions(&buffer, point..point, None, cx)
                })
                .await;
            if cancel.load(Ordering::Acquire) {
                return;
            }
            let actions = match actions {
                Ok(actions) => actions.unwrap_or_default(),
                Err(error) => {
                    log::warn!("code actions request failed: {error:#}");
                    Vec::new()
                }
            };
            let _ = events
                .send(Event::Feature(FeatureEvent::Language(
                    LanguageEvent::CodeActions {
                        generation,
                        actions,
                    },
                )))
                .await;
        })
        .detach();
    }

    /// An entry of a picker this feature owns was accepted.
    pub fn pick(&mut self, ctx: &mut Ctx, title: &str, index: usize, cx: &mut AsyncApp) {
        match title {
            COMPLETIONS => self.apply_completion(ctx, index, cx),
            CODE_ACTIONS => self.apply_code_action(ctx, index, cx),
            _ => {}
        }
    }

    pub fn update(&mut self, ctx: &mut Ctx, event: LanguageEvent) -> LanguageOutcome {
        let generation = match &event {
            LanguageEvent::Completions { generation, .. }
            | LanguageEvent::Hover { generation, .. }
            | LanguageEvent::Locations { generation, .. }
            | LanguageEvent::Diagnostics { generation, .. }
            | LanguageEvent::RenamePrepared { generation, .. }
            | LanguageEvent::CodeActions { generation, .. }
            | LanguageEvent::Applied { generation, .. } => *generation,
        };
        if generation != self.generation || self.item != Some(ctx.workspace.active_item()) {
            return LanguageOutcome::Consumed;
        }
        match event {
            LanguageEvent::Completions {
                query, completions, ..
            } => {
                if completions.is_empty() {
                    ctx.status.set("no completions");
                    return LanguageOutcome::Consumed;
                }
                let entries = completions
                    .iter()
                    .enumerate()
                    .map(|(index, completion)| PickerEntry {
                        label: completion.label.text.clone(),
                        detail: match &completion.source {
                            CompletionSource::Lsp { lsp_completion, .. } => {
                                lsp_completion.detail.clone().unwrap_or_default()
                            }
                            _ => String::new(),
                        },
                        enabled: true,
                        payload: PickerPayload::Index(index),
                    })
                    .collect();
                self.completions = Rc::new(RefCell::new(completions.into_boxed_slice()));
                let mut list = PickerList::new(entries);
                list.filter(&query);
                ctx.overlays.clear();
                ctx.overlays.push(Overlay::Picker {
                    title: COMPLETIONS,
                    query: LinePrompt::with_text(&query),
                    list,
                    owner: PickerOwner::Language,
                });
            }
            LanguageEvent::Hover { lines, .. } => {
                if lines.is_empty() {
                    ctx.status.set("no hover information");
                    return LanguageOutcome::Consumed;
                }
                ctx.overlays.clear();
                ctx.overlays.push(Overlay::Text {
                    title: HOVER,
                    rows: lines,
                    first: 0,
                });
            }
            LanguageEvent::Locations { kind, mut hits, .. } => {
                if hits.is_empty() {
                    ctx.status.set(kind.none());
                    return LanguageOutcome::Consumed;
                }
                if hits.len() == 1 {
                    return LanguageOutcome::Open(hits.remove(0).0);
                }
                let entries = hits
                    .into_iter()
                    .map(|(hit, label)| PickerEntry {
                        label,
                        detail: String::new(),
                        enabled: true,
                        payload: PickerPayload::Location {
                            path: hit.path,
                            row: hit.row,
                            column: hit.column,
                        },
                    })
                    .collect();
                ctx.overlays.clear();
                ctx.overlays.push(Overlay::Picker {
                    title: kind.title(),
                    query: LinePrompt::new(),
                    list: PickerList::new(entries),
                    owner: PickerOwner::Language,
                });
            }
            LanguageEvent::Diagnostics { entries, .. } => {
                if entries.is_empty() {
                    ctx.status.set("no diagnostics");
                    return LanguageOutcome::Consumed;
                }
                ctx.overlays.clear();
                ctx.overlays.push(Overlay::Picker {
                    title: DIAGNOSTICS,
                    query: LinePrompt::new(),
                    list: PickerList::new(entries),
                    owner: PickerOwner::Language,
                });
            }
            LanguageEvent::RenamePrepared { point, current, .. } => {
                let Some(current) = current else {
                    ctx.status.set("nothing to rename at the caret");
                    return LanguageOutcome::Consumed;
                };
                self.rename_point = Some(point);
                ctx.overlays.clear();
                ctx.overlays.push(Overlay::Prompt {
                    label: RENAME,
                    line: LinePrompt::with_text(&current),
                    target: PromptTarget::Rename,
                    feedback: None,
                });
            }
            LanguageEvent::CodeActions { actions, .. } => {
                if actions.is_empty() {
                    ctx.status.set("no code actions");
                    return LanguageOutcome::Consumed;
                }
                let entries = actions
                    .iter()
                    .enumerate()
                    .map(|(index, action)| PickerEntry {
                        label: action.lsp_action.title().to_owned(),
                        detail: String::new(),
                        enabled: true,
                        payload: PickerPayload::Index(index),
                    })
                    .collect();
                self.actions = actions;
                ctx.overlays.clear();
                ctx.overlays.push(Overlay::Picker {
                    title: CODE_ACTIONS,
                    query: LinePrompt::new(),
                    list: PickerList::new(entries),
                    owner: PickerOwner::Language,
                });
            }
            LanguageEvent::Applied { result, .. } => match result {
                Ok(message) => ctx.status.set(message),
                Err(error) => ctx.status.set(error),
            },
        }
        LanguageOutcome::Consumed
    }

    /// Applies the picked completion through the editor, then lets the
    /// server add its extra edits, such as imports.
    fn apply_completion(&mut self, ctx: &mut Ctx, index: usize, cx: &mut AsyncApp) {
        let Some(buffer) = self.active_buffer(ctx) else {
            return;
        };
        let completion = self.completions.borrow().get(index).cloned();
        let Some(completion) = completion else {
            return;
        };
        ctx.overlays.clear();
        if let Err(error) = zed::editor::apply_completion(&ctx.editor, &completion, cx) {
            ctx.status.set(format!("completion failed: {error:#}"));
            return;
        }
        ctx.status
            .set(format!("completed {}", completion.label.text));
        let lsp_store = ctx
            .services
            .project
            .read_with(cx, |project, _| project.lsp_store());
        let completions = self.completions.clone();
        let commit_range = completion.replace_range.clone();
        cx.spawn(async move |cx| {
            let resolved = lsp_store
                .update(cx, |lsp_store, cx| {
                    lsp_store.resolve_completions(
                        buffer.clone(),
                        vec![index],
                        completions.clone(),
                        cx,
                    )
                })
                .await;
            if let Err(error) = resolved {
                log::warn!("completion resolve failed: {error:#}");
            }
            let applied = lsp_store
                .update(cx, |lsp_store, cx| {
                    lsp_store.apply_additional_edits_for_completion(
                        buffer,
                        completions,
                        index,
                        true,
                        vec![commit_range],
                        cx,
                    )
                })
                .await;
            if let Err(error) = applied {
                log::warn!("completion additional edits failed: {error:#}");
            }
        })
        .detach();
    }

    fn apply_code_action(&mut self, ctx: &mut Ctx, index: usize, cx: &mut AsyncApp) {
        let Some(buffer) = self.active_buffer(ctx) else {
            return;
        };
        let Some(action) = self.actions.get(index).cloned() else {
            return;
        };
        ctx.overlays.clear();
        self.next_generation(ctx);
        let generation = self.generation;
        let title = action.lsp_action.title().to_owned();
        let project = ctx.services.project.clone();
        let events = ctx.events.clone();
        cx.spawn(async move |cx| {
            let result = project
                .update(cx, |project, cx| {
                    project.apply_code_action(buffer, action, true, cx)
                })
                .await
                .map(|_| format!("applied {title}"))
                .map_err(|error| format!("code action failed: {error:#}"));
            let _ = events
                .send(Event::Feature(FeatureEvent::Language(
                    LanguageEvent::Applied { generation, result },
                )))
                .await;
        })
        .detach();
    }

    /// Starts a request for the caret of the active document: the buffer
    /// and the point, with the previous request cancelled.
    fn begin(&mut self, ctx: &mut Ctx, cx: &mut AsyncApp) -> Option<(Entity<Buffer>, Point)> {
        let buffer = self.active_buffer(ctx)?;
        let point = match zed::editor::caret_point(&ctx.editor, cx) {
            Ok(point) => point,
            Err(error) => {
                ctx.status.set(format!("{error:#}"));
                return None;
            }
        };
        self.next_generation(ctx);
        Some((buffer, point))
    }

    fn next_generation(&mut self, ctx: &Ctx) {
        self.cancel.store(true, Ordering::Release);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.generation += 1;
        self.item = Some(ctx.workspace.active_item());
    }

    fn active_buffer(&self, ctx: &Ctx) -> Option<Entity<Buffer>> {
        ctx.documents
            .get(ctx.workspace.active_item())
            .map(|document| document.buffer.clone())
    }
}

fn severity(severity: lsp::DiagnosticSeverity) -> &'static str {
    if severity == lsp::DiagnosticSeverity::ERROR {
        "error"
    } else if severity == lsp::DiagnosticSeverity::WARNING {
        "warning"
    } else if severity == lsp::DiagnosticSeverity::INFORMATION {
        "info"
    } else {
        "hint"
    }
}
