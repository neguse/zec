use std::{env, ops::Range, sync::Arc, time::Duration};

use agent_settings::AgentSettings;
use anyhow::{Context as _, Result, ensure};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use editor::Editor;
use futures::StreamExt as _;
use gpui::{App, Context, Entity, Task};
use language::{Buffer, BufferEditSource, Capability, Point, ToOffset as _, TransactionId};
use language_model::{
    CompletionIntent, LanguageModelRegistry, LanguageModelRequest, LanguageModelRequestMessage,
    Role,
};
use multi_buffer::MultiBufferRow;
use prompt_store::PromptBuilder;
use similar::{ChangeTag, TextDiff};
use unicode_width::UnicodeWidthStr as _;

use crate::{
    prompt::{LinePrompt, PromptAction},
    render::{OverlayRow, OverlaySnapshot},
    terminal::TerminalEvent,
};

const MAX_PREVIEW_ROWS: usize = 160;
const MAX_PREVIEW_LINE_CHARS: usize = 500;

#[derive(Clone)]
pub struct InlineAssistTarget {
    buffer: Entity<Buffer>,
    range: Range<language::Anchor>,
    original: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InlineAssistPhase {
    Prompt,
    Generating,
    Preview,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InlineAssistInput {
    Consumed,
    Start,
    Accept,
    Reject,
    Close,
}

pub struct InlineAssistantState {
    target: InlineAssistTarget,
    prompt: LinePrompt,
    phase: InlineAssistPhase,
    generation: u64,
    generated: String,
    replacement: Option<String>,
    model_name: Option<String>,
    error: Option<String>,
    transaction: Option<TransactionId>,
    generation_task: Option<Task<()>>,
}

impl std::fmt::Debug for InlineAssistantState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InlineAssistantState")
            .field("phase", &self.phase)
            .field("generation", &self.generation)
            .field("generated_bytes", &self.generated.len())
            .field("has_transaction", &self.transaction.is_some())
            .finish()
    }
}

impl InlineAssistantState {
    pub fn capture(editor: &mut Editor, cx: &mut Context<Editor>) -> Result<Self> {
        ensure!(!editor.read_only(cx), "the active editor is read-only");

        let display = editor.display_snapshot(cx);
        let mut selection = editor.selections.newest::<Point>(&display);
        let multi_buffer = editor.buffer().clone();
        let snapshot = multi_buffer.read(cx).snapshot(cx);

        // Match Zed's buffer inline-assistant scope: a caret rewrites its
        // current line; a selection expands to complete lines.
        selection.start.column = 0;
        if selection.end.column == 0 && selection.start.row != selection.end.row {
            selection.end.row = selection.end.row.saturating_sub(1);
        }
        selection.end.column = snapshot.line_len(MultiBufferRow(selection.end.row));

        let mut ranges = snapshot
            .range_to_buffer_ranges(selection.start..selection.end)
            .into_iter();
        let (source_snapshot, source_range, _) = ranges
            .next()
            .context("the inline-assistant range is outside a source buffer")?;
        ensure!(
            ranges.next().is_none(),
            "an inline-assistant transformation cannot span multiple source buffers"
        );
        let source = multi_buffer
            .read(cx)
            .buffer(source_snapshot.remote_id())
            .context("the inline-assistant source buffer disappeared")?;
        ensure!(
            source.read(cx).capability() == Capability::ReadWrite,
            "the inline-assistant source buffer is read-only"
        );
        let original = source_snapshot
            .text_for_range(source_range.clone())
            .collect::<String>();
        let range = source_snapshot.anchor_before(source_range.start)
            ..source_snapshot.anchor_after(source_range.end);

        Ok(Self {
            target: InlineAssistTarget {
                buffer: source,
                range,
                original,
            },
            prompt: LinePrompt::new(),
            phase: InlineAssistPhase::Prompt,
            generation: 0,
            generated: String::new(),
            replacement: None,
            model_name: None,
            error: None,
            transaction: None,
            generation_task: None,
        })
    }

    pub fn phase(&self) -> InlineAssistPhase {
        self.phase
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn handle_key(&mut self, key: &KeyEvent) -> InlineAssistInput {
        if key.kind == KeyEventKind::Release {
            return InlineAssistInput::Consumed;
        }
        match self.phase {
            InlineAssistPhase::Prompt => match self.prompt.handle_key(key) {
                PromptAction::Submit | PromptAction::AlternateSubmit => {
                    if self.prompt.text().trim().is_empty() {
                        self.error = Some("enter an instruction".to_owned());
                        InlineAssistInput::Consumed
                    } else {
                        InlineAssistInput::Start
                    }
                }
                PromptAction::Cancel => InlineAssistInput::Close,
                PromptAction::Changed => {
                    self.error = None;
                    InlineAssistInput::Consumed
                }
                PromptAction::CursorMoved
                | PromptAction::Next
                | PromptAction::Previous
                | PromptAction::Ignored => InlineAssistInput::Consumed,
            },
            InlineAssistPhase::Generating => {
                if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::NONE {
                    InlineAssistInput::Close
                } else {
                    InlineAssistInput::Consumed
                }
            }
            InlineAssistPhase::Preview => {
                if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Enter {
                    InlineAssistInput::Accept
                } else if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Esc {
                    InlineAssistInput::Reject
                } else {
                    InlineAssistInput::Consumed
                }
            }
            InlineAssistPhase::Failed => {
                if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Enter {
                    InlineAssistInput::Start
                } else if key.modifiers == KeyModifiers::NONE && key.code == KeyCode::Esc {
                    InlineAssistInput::Close
                } else {
                    InlineAssistInput::Consumed
                }
            }
        }
    }

    pub fn handle_paste(&mut self, text: &str) -> InlineAssistInput {
        if self.phase == InlineAssistPhase::Prompt {
            self.prompt.handle_paste(text);
            self.error = None;
        }
        InlineAssistInput::Consumed
    }

    pub fn begin_generation(&mut self) -> (u64, InlineAssistTarget, String) {
        self.generation = self.generation.wrapping_add(1).max(1);
        self.phase = InlineAssistPhase::Generating;
        self.generated.clear();
        self.replacement = None;
        self.model_name = None;
        self.error = None;
        self.transaction = None;
        self.generation_task.take();
        (
            self.generation,
            self.target.clone(),
            self.prompt.text().to_owned(),
        )
    }

    pub fn set_generation_task(&mut self, model_name: String, task: Task<()>) {
        self.model_name = Some(model_name);
        self.generation_task = Some(task);
    }

    pub fn append_chunk(&mut self, generation: u64, chunk: &str) -> bool {
        if self.phase != InlineAssistPhase::Generating || self.generation != generation {
            return false;
        }
        self.generated.push_str(chunk);
        true
    }

    pub fn finish(
        &mut self,
        generation: u64,
        result: std::result::Result<String, String>,
        cx: &mut gpui::AsyncApp,
    ) -> Result<bool> {
        if self.phase != InlineAssistPhase::Generating || self.generation != generation {
            return Ok(false);
        }
        self.generation_task.take();
        let replacement = match result {
            Ok(replacement) => strip_markdown_fence(replacement),
            Err(error) => {
                self.phase = InlineAssistPhase::Failed;
                self.error = Some(error);
                return Ok(true);
            }
        };

        let target = self.target.clone();
        let transaction = target
            .buffer
            .update(cx, |buffer, cx| -> Result<_> {
                let snapshot = buffer.snapshot();
                let start = target.range.start.to_offset(&snapshot);
                let end = target.range.end.to_offset(&snapshot);
                let current = snapshot.text_for_range(start..end).collect::<String>();
                ensure!(
                    current == target.original,
                    "source changed while the inline assistant was generating"
                );
                buffer.finalize_last_transaction();
                buffer.start_transaction();
                buffer.edit(
                    [(target.range.clone(), Arc::<str>::from(replacement.clone()))],
                    None,
                    cx,
                );
                let transaction = buffer.end_transaction_with_source(BufferEditSource::Agent, cx);
                buffer.finalize_last_transaction();
                Ok(transaction)
            })?
            .context("inline-assistant replacement did not create a transaction")?;

        self.generated = replacement.clone();
        self.replacement = Some(replacement);
        self.transaction = Some(transaction);
        self.phase = InlineAssistPhase::Preview;
        Ok(true)
    }

    pub fn fail(&mut self, error: impl Into<String>) {
        self.generation_task.take();
        self.phase = InlineAssistPhase::Failed;
        self.error = Some(error.into());
    }

    pub fn reject(&mut self, cx: &mut gpui::AsyncApp) -> Result<()> {
        self.generation_task.take();
        if let Some(transaction) = self.transaction.take() {
            let undone = self
                .target
                .buffer
                .update(cx, |buffer, cx| buffer.undo_transaction(transaction, cx));
            ensure!(
                undone,
                "inline-assistant preview transaction is no longer undoable"
            );
        }
        Ok(())
    }

    pub fn cancel(&mut self) {
        self.generation_task.take();
    }

    pub fn status(&self, message: Option<&str>) -> (String, usize) {
        let message = message.map_or_else(String::new, |message| format!("{message}  |  "));
        match self.phase {
            InlineAssistPhase::Prompt => {
                let prefix = format!("{message}Inline Assistant: ");
                let cursor = prefix.width()
                    + self
                        .prompt
                        .text()
                        .get(..self.prompt.cursor())
                        .unwrap_or_default()
                        .width();
                let mut status =
                    format!("{prefix}{}  Enter generate  Esc cancel", self.prompt.text());
                if let Some(error) = &self.error {
                    status.push_str("  |  ");
                    status.push_str(error);
                }
                (status, cursor)
            }
            InlineAssistPhase::Generating => (
                format!(
                    "{message}Inline Assistant generating{}… {} bytes  Esc cancel",
                    self.model_name
                        .as_deref()
                        .map(|model| format!(" with {model}"))
                        .unwrap_or_default(),
                    self.generated.len()
                ),
                0,
            ),
            InlineAssistPhase::Preview => (
                format!(
                    "{message}Inline Assistant preview  Enter accept  Esc reject  Ctrl-Z undo after accept"
                ),
                0,
            ),
            InlineAssistPhase::Failed => (
                format!(
                    "{message}Inline Assistant failed: {}  Enter retry  Esc close",
                    self.error.as_deref().unwrap_or("unknown error")
                ),
                0,
            ),
        }
    }

    pub fn has_status_cursor(&self) -> bool {
        self.phase == InlineAssistPhase::Prompt
    }

    pub fn overlay(&self) -> OverlaySnapshot {
        let mut rows = vec![OverlayRow {
            text: format!("Instruction: {}", self.prompt.text()),
            enabled: true,
        }];
        match self.phase {
            InlineAssistPhase::Prompt => {
                rows.push(OverlayRow {
                    text: "The current line or selected complete lines will be rewritten."
                        .to_owned(),
                    enabled: false,
                });
                for line in self.target.original.lines().take(12) {
                    rows.push(OverlayRow {
                        text: format!("  {}", bounded_line(line)),
                        enabled: false,
                    });
                }
            }
            InlineAssistPhase::Generating => {
                rows.push(OverlayRow {
                    text: format!(
                        "Streaming{}…",
                        self.model_name
                            .as_deref()
                            .map(|model| format!(" from {model}"))
                            .unwrap_or_default()
                    ),
                    enabled: false,
                });
                for line in self
                    .generated
                    .lines()
                    .rev()
                    .take(24)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                {
                    rows.push(OverlayRow {
                        text: format!("+ {}", bounded_line(line)),
                        enabled: true,
                    });
                }
            }
            InlineAssistPhase::Preview => {
                rows.push(OverlayRow {
                    text: "Preview (not yet accepted):".to_owned(),
                    enabled: false,
                });
                let replacement = self.replacement.as_deref().unwrap_or_default();
                for change in TextDiff::from_lines(self.target.original.as_str(), replacement)
                    .iter_all_changes()
                    .take(MAX_PREVIEW_ROWS.saturating_sub(rows.len()))
                {
                    let (prefix, enabled) = match change.tag() {
                        ChangeTag::Delete => ("-", false),
                        ChangeTag::Insert => ("+", true),
                        ChangeTag::Equal => (" ", false),
                    };
                    rows.push(OverlayRow {
                        text: format!(
                            "{prefix} {}",
                            bounded_line(change.value().trim_end_matches('\n'))
                        ),
                        enabled,
                    });
                }
            }
            InlineAssistPhase::Failed => rows.push(OverlayRow {
                text: self
                    .error
                    .clone()
                    .unwrap_or_else(|| "unknown error".to_owned()),
                enabled: false,
            }),
        }
        rows.truncate(MAX_PREVIEW_ROWS);
        OverlaySnapshot {
            title: match self.phase {
                InlineAssistPhase::Prompt => "Inline Assistant".to_owned(),
                InlineAssistPhase::Generating => "Inline Assistant · Streaming".to_owned(),
                InlineAssistPhase::Preview => "Inline Assistant · Diff Preview".to_owned(),
                InlineAssistPhase::Failed => "Inline Assistant · Error".to_owned(),
            },
            rows,
            selected: None,
        }
    }
}

pub fn start_completion(
    target: InlineAssistTarget,
    user_prompt: String,
    generation: u64,
    prompt_builder: Arc<PromptBuilder>,
    event_sender: async_channel::Sender<TerminalEvent>,
    cx: &mut App,
) -> Result<(String, Task<()>)> {
    if let Some(response) = env::var_os("ZEC_INLINE_ASSIST_FIXTURE_RESPONSE") {
        let response = response.to_string_lossy().into_owned();
        let executor = cx.background_executor().clone();
        let task = cx.spawn(async move |_cx| {
            let mut complete = String::new();
            let mut chunk = String::new();
            for character in response.chars() {
                chunk.push(character);
                if chunk.chars().count() >= 8 {
                    complete.push_str(&chunk);
                    if event_sender
                        .send(TerminalEvent::InlineAssistChunk {
                            generation,
                            chunk: std::mem::take(&mut chunk),
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    executor.timer(Duration::from_millis(5)).await;
                }
            }
            if !chunk.is_empty() {
                complete.push_str(&chunk);
                if event_sender
                    .send(TerminalEvent::InlineAssistChunk { generation, chunk })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            let _ = event_sender
                .send(TerminalEvent::InlineAssistFinished {
                    generation,
                    result: Ok(complete),
                })
                .await;
        });
        return Ok(("fixture".to_owned(), task));
    }

    if user_prompt.trim().eq_ignore_ascii_case("delete") {
        let task = cx.spawn(async move |_cx| {
            let _ = event_sender
                .send(TerminalEvent::InlineAssistFinished {
                    generation,
                    result: Ok(String::new()),
                })
                .await;
        });
        return Ok(("local delete".to_owned(), task));
    }

    let configured = LanguageModelRegistry::read_global(cx)
        .inline_assistant_model()
        .context("no inline assistant model is configured; use Agent /models or settings.json")?;
    let model = configured.model;
    let snapshot = target.buffer.read(cx).snapshot();
    let start = target.range.start.to_offset(&snapshot);
    let end = target.range.end.to_offset(&snapshot);
    let language_name = snapshot.language().map(|language| language.name());
    let prompt = prompt_builder
        .generate_inline_transformation_prompt(
            user_prompt,
            language_name.as_ref(),
            snapshot,
            start..end,
        )
        .context("generate Zed inline transformation prompt")?;
    let temperature = AgentSettings::temperature_for_model(&model, cx);
    let request = LanguageModelRequest {
        thread_id: None,
        prompt_id: None,
        intent: Some(CompletionIntent::InlineAssist),
        messages: vec![LanguageModelRequestMessage {
            role: Role::User,
            content: vec![prompt.into()],
            cache: false,
            reasoning_details: None,
        }],
        tools: Vec::new(),
        tool_choice: None,
        stop: Vec::new(),
        temperature,
        thinking_allowed: false,
        thinking_effort: None,
        speed: None,
        compact_at_tokens: None,
    };
    let model_name = model.telemetry_id().to_string();
    let task = cx.spawn(async move |cx| {
        let result = async {
            let text_stream = model
                .stream_completion_text(request, cx)
                .await
                .map_err(anyhow::Error::from)?;
            let mut stream = text_stream.stream;
            let mut complete = String::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(anyhow::Error::from)?;
                complete.push_str(&chunk);
                event_sender
                    .send(TerminalEvent::InlineAssistChunk { generation, chunk })
                    .await
                    .context("terminal event loop closed during inline generation")?;
            }
            anyhow::Ok(complete)
        }
        .await;
        let _ = event_sender
            .send(TerminalEvent::InlineAssistFinished {
                generation,
                result: result.map_err(|error| format!("{error:#}")),
            })
            .await;
    });
    Ok((model_name, task))
}

fn strip_markdown_fence(mut response: String) -> String {
    let trimmed = response.trim();
    if !trimmed.starts_with("```") || !trimmed.ends_with("```") {
        return response;
    }
    let Some(first_newline) = trimmed.find('\n') else {
        return response;
    };
    let body = &trimmed[first_newline + 1..trimmed.len().saturating_sub(3)];
    response = body.strip_suffix('\n').unwrap_or(body).to_owned();
    response
}

fn bounded_line(line: &str) -> String {
    let mut output = line
        .chars()
        .take(MAX_PREVIEW_LINE_CHARS)
        .collect::<String>();
    if line.chars().count() > MAX_PREVIEW_LINE_CHARS {
        output.push('…');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_only_complete_markdown_fences() {
        assert_eq!(
            strip_markdown_fence("```rust\nfn main() {}\n```".to_owned()),
            "fn main() {}"
        );
        assert_eq!(strip_markdown_fence("plain".to_owned()), "plain");
    }
}
