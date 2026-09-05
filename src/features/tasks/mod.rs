//! Tasks: Zed's task inventory, run in the terminal panel.
//!
//! The inventory owns discovery (`.zed/tasks.json` in the root and the
//! user's tasks file) and resolution against the worktree and the active
//! buffer; zec lists the resolved tasks in a picker and hands the spawned
//! terminal to the terminal panel.

use std::{path::Path, sync::Arc};

use anyhow::{Context as _, Result};
use gpui::{AsyncApp, Entity};
use project::{TaskContexts, TaskSourceKind};
use task::{ResolvedTask, TaskContext, TaskVariables, VariableName};
use zed_terminal::Terminal;

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

const TITLE: &str = "Tasks";

#[derive(Debug)]
pub enum TasksEvent {
    Finished { label: String, success: bool },
}

#[derive(Default)]
pub struct Tasks {
    /// The entries of the open picker, resolved by `Index`.
    candidates: Vec<(TaskSourceKind, ResolvedTask)>,
}

impl Tasks {
    /// Lists the tasks the inventory resolves for the root and the active
    /// buffer, most recently run first.
    pub async fn run(&mut self, ctx: &mut Ctx<'_>, cx: &mut AsyncApp) {
        let Some(worktree) = ctx.services.visible_worktree(cx) else {
            ctx.status.set("tasks need a directory root");
            return;
        };
        let Some(inventory) = ctx.services.task_inventory(cx) else {
            ctx.status.set("tasks are unavailable");
            return;
        };
        let (id, abs_path) = worktree.read_with(cx, |worktree, _| {
            (worktree.id(), worktree.abs_path().to_path_buf())
        });
        let mut contexts = TaskContexts::default();
        let editor_context = ctx
            .editor
            .update(cx, |editor, window, cx| editor.task_context(window, cx))
            .ok();
        if let Some(editor_context) = editor_context
            && let Some(context) = editor_context.await
        {
            contexts.active_item_context = Some((Some(id), None, context));
        }
        contexts.active_worktree_context = Some((id, worktree_context(&abs_path)));
        let (used, current) = inventory
            .update(cx, |inventory, cx| {
                inventory.used_and_current_resolved_tasks(Arc::new(contexts), cx)
            })
            .await;
        let mut candidates = used;
        for candidate in current {
            if !candidates.iter().any(|(_, task)| task.id == candidate.1.id) {
                candidates.push(candidate);
            }
        }
        if candidates.is_empty() {
            ctx.status
                .set("no tasks; define them in .zed/tasks.json under the root");
            return;
        }
        let entries = candidates
            .iter()
            .enumerate()
            .map(|(index, (_, task))| PickerEntry {
                label: task.resolved_label.clone(),
                detail: task.resolved.command_label.clone(),
                enabled: true,
                payload: PickerPayload::Index(index),
            })
            .collect();
        self.candidates = candidates;
        ctx.overlays.clear();
        ctx.overlays.push(Overlay::Picker {
            title: TITLE,
            query: LinePrompt::new(),
            list: PickerList::new(entries),
            owner: PickerOwner::Tasks,
        });
    }

    /// Runs the picked task; the terminal it runs in is the caller's to
    /// show.
    pub async fn pick(
        &mut self,
        ctx: &mut Ctx<'_>,
        index: usize,
        cx: &mut AsyncApp,
    ) -> Option<Entity<Terminal>> {
        let Some((kind, task)) = self.candidates.get(index).cloned() else {
            return None;
        };
        ctx.overlays.pop();
        self.spawn(ctx, kind, task, cx).await
    }

    /// Runs the task the inventory last scheduled.
    pub async fn rerun(
        &mut self,
        ctx: &mut Ctx<'_>,
        cx: &mut AsyncApp,
    ) -> Option<Entity<Terminal>> {
        let Some(inventory) = ctx.services.task_inventory(cx) else {
            ctx.status.set("tasks are unavailable");
            return None;
        };
        let Some((kind, task)) =
            inventory.read_with(cx, |inventory, _| inventory.last_scheduled_task(None))
        else {
            ctx.status.set("no task has run yet");
            return None;
        };
        self.spawn(ctx, kind, task, cx).await
    }

    pub fn update(&mut self, ctx: &mut Ctx, event: TasksEvent) {
        let TasksEvent::Finished { label, success } = event;
        ctx.status.set(format!(
            "task {label} {}",
            if success { "finished" } else { "failed" }
        ));
    }

    async fn spawn(
        &mut self,
        ctx: &mut Ctx<'_>,
        kind: TaskSourceKind,
        task: ResolvedTask,
        cx: &mut AsyncApp,
    ) -> Option<Entity<Terminal>> {
        let label = task.resolved_label.clone();
        match start(ctx, kind, task, cx).await {
            Ok(terminal) => {
                ctx.status.set(format!("running task {label}"));
                Some(terminal)
            }
            Err(error) => {
                ctx.status
                    .set(format!("task {label} failed to start: {error:#}"));
                None
            }
        }
    }
}

async fn start(
    ctx: &mut Ctx<'_>,
    kind: TaskSourceKind,
    task: ResolvedTask,
    cx: &mut AsyncApp,
) -> Result<Entity<Terminal>> {
    let inventory = ctx
        .services
        .task_inventory(cx)
        .context("tasks are unavailable")?;
    inventory.update(cx, |inventory, _| {
        inventory.task_scheduled(kind, task.clone());
    });
    let spawn = task.resolved.clone();
    let terminal = ctx
        .services
        .project
        .update(cx, |project, cx| project.create_terminal_task(spawn, cx))
        .await?;
    // The exit status arrives as an event, so the status row can report it
    // even while the panel is hidden.
    let completed = terminal.read_with(cx, |terminal, cx| terminal.wait_for_completed_task(cx));
    let events = ctx.events.clone();
    let label = task.resolved_label;
    cx.spawn(async move |_| {
        let status = completed.await;
        let success = status.is_some_and(|status| status.success());
        let _ = events
            .send(Event::Feature(FeatureEvent::Tasks(TasksEvent::Finished {
                label,
                success,
            })))
            .await;
    })
    .detach();
    Ok(terminal)
}

/// The context Zed's task UI gives a worktree: its root as the working
/// directory and as `$ZED_WORKTREE_ROOT`.
fn worktree_context(root: &Path) -> TaskContext {
    let mut task_variables = TaskVariables::default();
    task_variables.insert(
        VariableName::WorktreeRoot,
        root.to_string_lossy().into_owned(),
    );
    TaskContext {
        cwd: Some(root.to_path_buf()),
        task_variables,
        project_env: Default::default(),
    }
}
