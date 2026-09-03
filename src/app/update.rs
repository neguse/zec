//! Routing and execution: every event passes through [`App::update`].

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use gpui::AsyncApp;
use ratatui::layout::Position;

use super::{
    App, Flow, OVERLAY_CONTEXT,
    command::Command,
    documents::Document,
    event::{ConfigEvent, DocumentEvent, Event, Input},
    feature::{Ctx, FeatureEvent, Features},
    overlay::{Overlay, PickerOwner, PickerPayload, PromptTarget},
    workspace::ItemId,
};
use crate::{
    terminal::{
        self, clipboard, keys,
        picker::{PickerEntry, PickerList},
        prompt::{LinePrompt, PromptAction},
        render::EditorWidget,
    },
    zed,
};

enum OverlayOutcome {
    Consumed,
    QueryChanged,
    Submit(PromptTarget, String),
    Cancel(&'static str),
    Command(Command),
    OpenPath(PathBuf),
}

impl App {
    pub async fn update(&mut self, event: Event, cx: &mut AsyncApp) -> Result<Flow> {
        let flow = match event {
            Event::Input(input) => self.route_input(input, cx).await?,
            Event::Resize(acknowledgement) => {
                self.resize = Some(acknowledgement);
                self.needs_invalidate = true;
                Flow::Continue
            }
            Event::Redraw => Flow::Continue,
            Event::FocusChanged(focused) => {
                self.capabilities.observe_focus();
                self.status.set(if focused {
                    "terminal focus gained"
                } else {
                    "terminal focus lost"
                });
                Flow::Continue
            }
            Event::Signal(signal) => {
                if terminal::is_suspend_signal(signal) {
                    Flow::Suspend
                } else {
                    Flow::Exit
                }
            }
            Event::Fatal(message) => bail!(message),
            Event::Document(DocumentEvent::ReloadFinished { buffer_id, result }) => {
                if self.documents.item_for_buffer_id(buffer_id, cx).is_some() {
                    match result {
                        Ok(()) => self.status.set("reloaded from disk"),
                        Err(error) => self.status.set(format!("reload failed: {error}")),
                    }
                }
                Flow::Continue
            }
            Event::Config(ConfigEvent { kind, result }) => {
                match result {
                    Ok(outcome) => self.status.set(format!("{kind} {outcome}")),
                    Err(error) => self.status.set(format!("{kind} reload failed: {error}")),
                }
                Flow::Continue
            }
            Event::Feature(FeatureEvent::QuickOpen(event)) => {
                let (features, mut ctx) = self.feature_ctx();
                features.quick_open.update(&mut ctx, event);
                Flow::Continue
            }
        };
        self.check_invariants()?;
        Ok(flow)
    }

    pub(super) fn shutdown(&mut self, cx: &mut AsyncApp) {
        for (_, document) in self.documents.iter() {
            let _ = zed::editor::close_window(&document.editor, cx);
        }
    }

    // ---- input routing ----

    async fn route_input(&mut self, input: Input, cx: &mut AsyncApp) -> Result<Flow> {
        match input {
            Input::Key(key) => self.route_key(key, cx).await,
            Input::Paste(text) => {
                if self.overlays.owns_input() {
                    if let Some(Overlay::Prompt { line, feedback, .. }) = self.overlays.top_mut() {
                        line.handle_paste(&text);
                        *feedback = None;
                    } else if let Some(Overlay::Picker { query, .. }) = self.overlays.top_mut() {
                        query.handle_paste(&text);
                        self.picker_query_changed(cx);
                    }
                    return Ok(Flow::Continue);
                }
                zed::editor::paste(&self.active_document().editor, &text, cx)?;
                self.input_reached_document();
                Ok(Flow::Continue)
            }
            Input::Mouse(mouse) => {
                self.capabilities.observe_mouse();
                self.route_mouse(mouse, cx)?;
                Ok(Flow::Continue)
            }
            Input::Scroll(direction) => {
                if self.overlays.owns_input() {
                    return Ok(Flow::Continue);
                }
                let Some(frame) = self.frame.as_ref() else {
                    return Ok(Flow::Continue);
                };
                let total_rows = frame.snapshot.total_rows;
                let body_height = usize::from(frame.area.height.saturating_sub(1));
                let document = self.active_document_mut();
                if document
                    .viewport
                    .scroll(total_rows, body_height, direction, 3)
                {
                    document.follow.manual_vertical_scroll = true;
                }
                self.status.clear();
                Ok(Flow::Continue)
            }
        }
    }

    async fn route_key(&mut self, key: KeyEvent, cx: &mut AsyncApp) -> Result<Flow> {
        if self.overlays.owns_input() {
            if let Some(keystroke) = keys::to_gpui_keystroke(key) {
                let command = self
                    .keymap
                    .borrow()
                    .resolve(&keystroke, OVERLAY_CONTEXT)
                    .into_iter()
                    .find_map(Command::from_action_name);
                if let Some(command) = command {
                    return self.execute(command, cx).await;
                }
            }
            return self.overlay_key(key, cx).await;
        }

        let Some(keystroke) = keys::to_gpui_keystroke(key) else {
            return Ok(Flow::Continue);
        };
        zed::editor::dispatch_keystroke(&self.active_document().editor, keystroke, cx)?;
        let commands = std::mem::take(&mut *self.pending_commands.borrow_mut());
        if commands.is_empty() {
            self.input_reached_document();
            return Ok(Flow::Continue);
        }
        for command in commands {
            match self.execute(command, cx).await? {
                Flow::Continue => {}
                flow => return Ok(flow),
            }
        }
        Ok(Flow::Continue)
    }

    /// Any input the document consumed resets a pending confirmation and
    /// the transient status.
    fn input_reached_document(&mut self) {
        self.overlays.dismiss_confirmation();
        self.status.clear();
    }

    async fn overlay_key(&mut self, key: KeyEvent, cx: &mut AsyncApp) -> Result<Flow> {
        let outcome = match self.overlays.top_mut() {
            Some(Overlay::Prompt {
                line,
                target,
                feedback,
                label,
            }) => match line.handle_key(&key) {
                PromptAction::Changed => {
                    *feedback = None;
                    if let PromptTarget::SaveAs { overwrite } = target {
                        *overwrite = None;
                    }
                    OverlayOutcome::Consumed
                }
                PromptAction::Submit | PromptAction::AlternateSubmit => {
                    OverlayOutcome::Submit(target.clone(), line.text().to_owned())
                }
                PromptAction::Cancel => OverlayOutcome::Cancel(label),
                PromptAction::CursorMoved
                | PromptAction::Next
                | PromptAction::Previous
                | PromptAction::Ignored => OverlayOutcome::Consumed,
            },
            Some(Overlay::Picker {
                title, query, list, ..
            }) => match query.handle_key(&key) {
                PromptAction::Changed => OverlayOutcome::QueryChanged,
                PromptAction::Next => {
                    list.select_next();
                    OverlayOutcome::Consumed
                }
                PromptAction::Previous => {
                    list.select_previous();
                    OverlayOutcome::Consumed
                }
                PromptAction::Submit | PromptAction::AlternateSubmit => {
                    match list.selected().filter(|entry| entry.enabled) {
                        Some(entry) => match entry.payload.clone() {
                            PickerPayload::Command(command) => OverlayOutcome::Command(command),
                            PickerPayload::Path(path) => OverlayOutcome::OpenPath(path),
                        },
                        None => OverlayOutcome::Consumed,
                    }
                }
                PromptAction::Cancel => OverlayOutcome::Cancel(title),
                PromptAction::CursorMoved | PromptAction::Ignored => OverlayOutcome::Consumed,
            },
            _ => OverlayOutcome::Consumed,
        };

        match outcome {
            OverlayOutcome::Consumed => Ok(Flow::Continue),
            OverlayOutcome::QueryChanged => {
                self.picker_query_changed(cx);
                Ok(Flow::Continue)
            }
            OverlayOutcome::OpenPath(path) => {
                self.overlays.pop();
                match self.open_document_at(path, cx).await {
                    Ok(message) => self.status.set(message),
                    Err(error) => self.status.set(format!("open failed: {error:#}")),
                }
                Ok(Flow::Continue)
            }
            OverlayOutcome::Cancel(label) => {
                self.overlays.pop();
                self.status
                    .set(format!("{} cancelled", label.to_lowercase()));
                Ok(Flow::Continue)
            }
            OverlayOutcome::Command(command) => {
                self.overlays.pop();
                self.execute(command, cx).await
            }
            OverlayOutcome::Submit(PromptTarget::SaveAs { overwrite }, text) => {
                self.submit_save_as(&text, overwrite.as_deref(), cx).await?;
                Ok(Flow::Continue)
            }
            OverlayOutcome::Submit(PromptTarget::OpenFile, text) => {
                match resolve_path(&self.cwd, &text) {
                    Ok(path) => match self.open_document_at(path, cx).await {
                        Ok(message) => {
                            self.overlays.pop();
                            self.status.set(message);
                        }
                        Err(error) => self.set_prompt_feedback(format!("open failed: {error:#}")),
                    },
                    Err(error) => self.set_prompt_feedback(format!("open failed: {error:#}")),
                }
                Ok(Flow::Continue)
            }
        }
    }

    fn route_mouse(&mut self, mouse: MouseEvent, cx: &mut AsyncApp) -> Result<()> {
        if self.overlays.owns_input()
            || mouse.kind != MouseEventKind::Down(MouseButton::Left)
            || !mouse.modifiers.is_empty()
        {
            return Ok(());
        }
        let position = Position::new(mouse.column, mouse.row);
        let Some(text_position) = self.frame.as_ref().and_then(|frame| {
            EditorWidget::new(&frame.snapshot).text_position_at(frame.area, position)
        }) else {
            return Ok(());
        };
        zed::editor::place_caret(&self.active_document().editor, text_position, cx)?;
        self.input_reached_document();
        Ok(())
    }

    // ---- commands ----

    /// Runs one command. A pending confirmation is settled here, in one
    /// place: repeating the confirmed command carries `confirmed`, and any
    /// other command dismisses it.
    async fn execute(&mut self, command: Command, cx: &mut AsyncApp) -> Result<Flow> {
        let confirmed = self.overlays.take_confirmation(command);
        match command {
            Command::Quit => return self.quit(confirmed, cx),
            Command::CloseItem => return self.close_item(confirmed, cx).await,
            Command::Save => self.save(confirmed, cx).await?,
            Command::SaveAs => {
                let current = self
                    .active_document()
                    .state(cx)
                    .path
                    .map(|path| path.display().to_string())
                    .unwrap_or_default();
                self.push_prompt(
                    "Save as",
                    PromptTarget::SaveAs { overwrite: None },
                    &current,
                );
            }
            Command::Reload => self.reload(confirmed, cx).await?,
            Command::NewFile => self.new_file(cx).await?,
            Command::OpenFile => self.push_prompt("Open", PromptTarget::OpenFile, ""),
            Command::QuickOpen => {
                let (features, mut ctx) = self.feature_ctx();
                features.quick_open.open(&mut ctx, cx);
            }
            Command::NextItem | Command::PreviousItem => {
                if self
                    .workspace
                    .activate_adjacent_item(command == Command::NextItem)
                {
                    self.item_changed();
                }
            }
            Command::CommandPalette => self.push_palette(cx),
            Command::ShowTerminalCapabilities => self.status.set(self.capabilities.summary()),
            Command::Copy => self.copy(false, cx)?,
            Command::Cut => self.copy(true, cx)?,
            Command::OpenSettings => {
                let message = self
                    .open_document_at(paths::settings_file().clone(), cx)
                    .await?;
                self.status.set(message);
            }
            Command::OpenKeymap => {
                let message = self
                    .open_document_at(paths::keymap_file().clone(), cx)
                    .await?;
                self.status.set(message);
            }
        }
        Ok(Flow::Continue)
    }

    fn quit(&mut self, confirmed: bool, cx: &mut AsyncApp) -> Result<Flow> {
        let guarded = self
            .documents
            .iter()
            .filter(|(_, document)| document.state(cx).needs_discard_confirmation())
            .count();
        if guarded == 0 || confirmed {
            return Ok(Flow::Exit);
        }
        self.overlays.push(Overlay::Confirm {
            message: format!(
                "{guarded} unsaved or deleted tab(s); press {} again to discard",
                self.key_hint(Command::Quit)
            ),
            command: Command::Quit,
        });
        Ok(Flow::Continue)
    }

    async fn close_item(&mut self, confirmed: bool, cx: &mut AsyncApp) -> Result<Flow> {
        let item = self.workspace.active_item();
        let state = self.active_document().state(cx);
        if state.needs_discard_confirmation() && !confirmed {
            self.overlays.push(Overlay::Confirm {
                message: format!(
                    "unsaved changes or deleted file; press {} again to discard this tab",
                    self.key_hint(Command::CloseItem)
                ),
                command: Command::CloseItem,
            });
            return Ok(Flow::Continue);
        }
        if self.documents.len() == 1 {
            return Ok(Flow::Exit);
        }
        let document = self
            .documents
            .remove(item)
            .context("active item has no document")?;
        zed::editor::close_window(&document.editor, cx)?;
        self.workspace
            .close_item(item)
            .context("close item through the workspace")?;
        self.item_changed();
        self.status.set("tab closed");
        Ok(Flow::Continue)
    }

    async fn save(&mut self, confirmed: bool, cx: &mut AsyncApp) -> Result<()> {
        let state = self.active_document().state(cx);
        if state.path.is_none() {
            self.push_prompt("Save as", PromptTarget::SaveAs { overwrite: None }, "");
            return Ok(());
        }
        if state.has_external_change() && !confirmed {
            let (save, reload) = (self.key_hint(Command::Save), self.key_hint(Command::Reload));
            self.overlays.push(Overlay::Confirm {
                message: if state.deleted {
                    format!(
                        "deleted on disk; press {save} again to recreate, or {reload} to reload"
                    )
                } else {
                    format!(
                        "changed on disk; press {save} again to overwrite, or {reload} to reload"
                    )
                },
                command: Command::Save,
            });
            return Ok(());
        }
        let buffer = self.active_document().buffer.clone();
        match self.services.save(&buffer, cx).await {
            Ok(()) => self.status.set("saved"),
            Err(error) => self.status.set(format!("save failed: {error:#}")),
        }
        Ok(())
    }

    async fn submit_save_as(
        &mut self,
        input: &str,
        overwrite: Option<&Path>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let path = match resolve_path(&self.cwd, input) {
            Ok(path) => path,
            Err(error) => {
                self.set_prompt_feedback(format!("save failed: {error:#}"));
                return Ok(());
            }
        };
        match self.services.metadata(&path).await {
            Ok(Some(metadata)) => {
                if metadata.is_dir {
                    self.set_prompt_feedback(format!("{} is a directory", path.display()));
                    return Ok(());
                }
                if metadata.is_fifo || !self.services.fs.is_file(&path).await {
                    self.set_prompt_feedback(format!("{} is not a regular file", path.display()));
                    return Ok(());
                }
                if overwrite != Some(path.as_path()) {
                    self.set_prompt_feedback("file exists; press Enter again to overwrite");
                    if let Some(Overlay::Prompt {
                        target: PromptTarget::SaveAs { overwrite },
                        ..
                    }) = self.overlays.top_mut()
                    {
                        *overwrite = Some(path);
                    }
                    return Ok(());
                }
            }
            Ok(None) => {}
            Err(error) => {
                self.set_prompt_feedback(format!("save failed: {error:#}"));
                return Ok(());
            }
        }
        let buffer = self.active_document().buffer.clone();
        match self.services.save_as(&buffer, &path, cx).await {
            Ok(()) => {
                self.overlays.pop();
                self.status.set("saved");
            }
            Err(error) => self.set_prompt_feedback(format!("save failed: {error:#}")),
        }
        Ok(())
    }

    async fn reload(&mut self, confirmed: bool, cx: &mut AsyncApp) -> Result<()> {
        let state = self.active_document().state(cx);
        if state.path.is_none() {
            self.status.set("reload failed: buffer has no file path");
            return Ok(());
        }
        if state.dirty && !confirmed {
            self.overlays.push(Overlay::Confirm {
                message: format!(
                    "unsaved changes; press {} again to reload from disk",
                    self.key_hint(Command::Reload)
                ),
                command: Command::Reload,
            });
            return Ok(());
        }
        let buffer = self.active_document().buffer.clone();
        match self.services.reload(&buffer, cx).await {
            Ok(()) if self.active_document().state(cx).conflict => self
                .status
                .set("file changed again while reloading; local edits were kept"),
            Ok(()) => self.status.set("reloaded from disk"),
            Err(error) => self.status.set(format!("reload failed: {error:#}")),
        }
        Ok(())
    }

    async fn new_file(&mut self, cx: &mut AsyncApp) -> Result<()> {
        let buffer = cx.update(|cx| self.services.create_scratch(cx));
        let label = self.documents.next_untitled_label();
        let document = Document::open(buffer, Some(label), &self.services, &self.events, cx)?;
        let item = self.documents.insert(document);
        self.show_item(item)?;
        self.status.set("new tab");
        Ok(())
    }

    /// Opens `path` in a tab, or activates the tab already showing it.
    async fn open_document_at(&mut self, path: PathBuf, cx: &mut AsyncApp) -> Result<String> {
        let buffer = self.services.open_file(&path, cx).await?;
        if let Some(item) = self.documents.item_for_buffer(&buffer) {
            self.workspace
                .focus_item(item)
                .context("activate existing tab")?;
            self.item_changed();
            return Ok(format!("switched to {}", self.display_path(&path)));
        }
        let document = Document::open(buffer, None, &self.services, &self.events, cx)?;
        let item = self.documents.insert(document);
        self.show_item(item)?;
        Ok(format!("opened {}", self.display_path(&path)))
    }

    fn copy(&mut self, cut: bool, cx: &mut AsyncApp) -> Result<()> {
        let editor = self.active_document().editor;
        let item = editor
            .update(cx, |editor, _window, cx| {
                if cut {
                    clipboard::item_for_cut(editor, cx)
                } else {
                    clipboard::item_for_copy(editor, cx)
                }
            })
            .context("read the selection for the clipboard")?;
        if let Err(error) = clipboard::write_osc52(&mut std::io::stdout(), &item) {
            self.status.set(format!(
                "{} failed: {error}",
                if cut { "cut" } else { "copy" }
            ));
            return Ok(());
        }
        if cut {
            zed::editor::dispatch_action(&editor, Box::new(editor::actions::Cut), cx)?;
            self.status.set("cut to terminal clipboard");
        } else {
            self.status.set("copied to terminal clipboard");
        }
        Ok(())
    }

    fn push_palette(&mut self, cx: &mut AsyncApp) {
        let has_file = self.active_document().state(cx).path.is_some();
        let entries = Command::ALL
            .iter()
            .map(|command| PickerEntry {
                label: command.label().to_owned(),
                detail: self
                    .keymap
                    .borrow()
                    .keystroke_for(command.action_name())
                    .map(|keystroke| keys::display_keystroke(&keystroke))
                    .unwrap_or_default(),
                enabled: !matches!(command, Command::Reload) || has_file,
                payload: PickerPayload::Command(*command),
            })
            .collect();
        self.overlays.clear();
        self.overlays.push(Overlay::Picker {
            title: "Commands",
            query: LinePrompt::new(),
            list: PickerList::new(entries),
            owner: PickerOwner::Palette,
        });
    }

    /// A picker's query changed: the palette filters its fixed entries, a
    /// feature-owned picker asks its feature for new ones.
    fn picker_query_changed(&mut self, cx: &mut AsyncApp) {
        let Some(Overlay::Picker {
            query, list, owner, ..
        }) = self.overlays.top_mut()
        else {
            return;
        };
        match owner {
            PickerOwner::Palette => list.filter(query.text()),
            PickerOwner::QuickOpen => {
                let query = query.text().to_owned();
                let (features, mut ctx) = self.feature_ctx();
                features.quick_open.query_changed(&mut ctx, &query, cx);
            }
        }
    }

    // ---- helpers ----

    /// Splits the tree into the features and what they may touch.
    fn feature_ctx(&mut self) -> (&mut Features, Ctx<'_>) {
        let Self {
            features,
            services,
            root,
            overlays,
            status,
            events,
            ..
        } = self;
        (
            features,
            Ctx {
                services,
                root: root.as_deref(),
                overlays,
                status,
                events,
            },
        )
    }

    /// Paths in messages are relative to the root when there is one.
    fn display_path(&self, path: &Path) -> String {
        self.root
            .as_deref()
            .and_then(|root| path.strip_prefix(root).ok())
            .unwrap_or(path)
            .display()
            .to_string()
    }

    fn push_prompt(&mut self, label: &'static str, target: PromptTarget, text: &str) {
        self.overlays.clear();
        self.overlays.push(Overlay::Prompt {
            label,
            line: LinePrompt::with_text(text),
            target,
            feedback: None,
        });
    }

    fn set_prompt_feedback(&mut self, feedback: impl Into<String>) {
        if let Some(Overlay::Prompt {
            feedback: slot,
            target,
            ..
        }) = self.overlays.top_mut()
        {
            *slot = Some(feedback.into());
            if let PromptTarget::SaveAs { overwrite } = target {
                *overwrite = None;
            }
        }
    }

    fn show_item(&mut self, item: ItemId) -> Result<()> {
        self.workspace
            .open_item(item)
            .context("open item in the workspace")?;
        self.item_changed();
        Ok(())
    }

    /// Overlays are bound to the item they were opened on.
    fn item_changed(&mut self) {
        self.overlays.clear();
        self.status.clear();
    }

    pub(super) fn active_document(&self) -> &Document {
        self.documents
            .get(self.workspace.active_item())
            .expect("the active item always has a document")
    }

    fn active_document_mut(&mut self) -> &mut Document {
        let item = self.workspace.active_item();
        self.documents
            .get_mut(item)
            .expect("the active item always has a document")
    }

    /// The key currently bound to `command`, as the status row shows it.
    pub(super) fn key_hint(&self, command: Command) -> String {
        self.keymap
            .borrow()
            .keystroke_for(command.action_name())
            .map(|keystroke| keys::display_keystroke(&keystroke))
            .unwrap_or_else(|| format!("`{}`", command.label()))
    }
}

/// Prompt paths resolve against the startup directory with no shell
/// expansion.
fn resolve_path(cwd: &Path, input: &str) -> Result<PathBuf> {
    if input.is_empty() {
        bail!("path is empty");
    }
    let input = PathBuf::from(input);
    let joined = if input.is_absolute() {
        input
    } else {
        cwd.join(input)
    };
    std::path::absolute(&joined)
        .with_context(|| format!("could not make {} absolute", joined.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_paths_resolve_against_cwd_without_expansion() {
        let cwd = Path::new("/work");
        assert_eq!(
            resolve_path(cwd, "a.txt").unwrap(),
            PathBuf::from("/work/a.txt")
        );
        assert_eq!(
            resolve_path(cwd, "~/a.txt").unwrap(),
            PathBuf::from("/work/~/a.txt")
        );
        assert_eq!(
            resolve_path(cwd, "/abs.txt").unwrap(),
            PathBuf::from("/abs.txt")
        );
        assert!(resolve_path(cwd, "").is_err());
    }
}
