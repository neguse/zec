//! Edit prediction: Zed's providers on zec's hidden Editors.
//!
//! Display and acceptance are the Editor's own: a prediction is an inlay in
//! the display snapshot, so it reaches the terminal as ghost text, and Zed's
//! actions accept it. zec only chooses the provider from Zed's language
//! settings, as Zed's workspace does, and reassigns it when they change.
//! `ZEC_EDIT_PREDICTION_FIXTURE=text` replaces the provider with a
//! deterministic one for the PTY tests; it still travels the real path.

use std::{cell::RefCell, env, rc::Rc, sync::Arc};

use client::{Client, UserStore};
use collections::HashMap;
use copilot::CopilotEditPredictionDelegate;
use edit_prediction::{EditPredictionModel, ZedEditPredictionDelegate};
use edit_prediction_types::{
    EditPrediction, EditPredictionDelegate, EditPredictionDiscardReason, EditPredictionIconSet,
    EditPredictionRequestTrigger,
};
use editor::Editor;
use gpui::{AnyWindowHandle, App, AppContext as _, Context, Entity, WeakEntity, Window};
use icons::IconName;
use language::{
    Buffer, ZetaVersion,
    language_settings::{
        EditPredictionPromptFormat, EditPredictionProvider, all_language_settings,
    },
};
use settings::SettingsStore;

use crate::zed::runtime::Runtime;

type Editors = Rc<RefCell<HashMap<WeakEntity<Editor>, AnyWindowHandle>>>;

/// Installs the provider registry; called once when GPUI starts.
pub fn init(cx: &mut App) {
    let runtime = cx.global::<Runtime>().clone();
    let (client, user_store) = (runtime.client, runtime.user_store);
    // The store listens for LLM token refreshes; Zed registers the listener
    // once at startup.
    client::RefreshLlmTokenListener::register(client.clone(), user_store.clone(), cx);
    edit_prediction::EditPredictionStore::global(&client, &user_store, cx);

    let editors: Editors = Rc::default();
    cx.observe_new({
        let editors = editors.clone();
        let client = client.clone();
        let user_store = user_store.clone();
        move |editor: &mut Editor, window, cx: &mut Context<Editor>| {
            let Some(window) = window else {
                return;
            };
            if !editor.mode().is_full() {
                return;
            }
            let handle = cx.entity().downgrade();
            cx.on_release({
                let handle = handle.clone();
                let editors = editors.clone();
                move |_, _| {
                    editors.borrow_mut().remove(&handle);
                }
            })
            .detach();
            editors.borrow_mut().insert(handle, window.window_handle());
            assign(
                editor,
                provider_for_settings(cx),
                EditPredictionRequestTrigger::EditorCreated,
                &client,
                user_store.clone(),
                window,
                cx,
            );
        }
    })
    .detach();

    cx.observe_global::<SettingsStore>({
        let mut previous = provider_for_settings(cx);
        move |cx| {
            let current = provider_for_settings(cx);
            if current == previous {
                return;
            }
            previous = current.clone();
            for (editor, window) in editors.borrow().iter() {
                let _ = window.update(cx, |_, window, cx| {
                    let _ = editor.update(cx, |editor, cx| {
                        assign(
                            editor,
                            current.clone(),
                            EditPredictionRequestTrigger::ProviderChanged,
                            &client,
                            user_store.clone(),
                            window,
                            cx,
                        );
                    });
                });
            }
        }
    })
    .detach();
}

#[derive(Clone, PartialEq, Eq)]
enum Provider {
    Fixture(Arc<str>),
    Copilot,
    Zed(EditPredictionModel),
}

fn provider_for_settings(cx: &App) -> Option<Provider> {
    if let Some(text) = env::var("ZEC_EDIT_PREDICTION_FIXTURE")
        .ok()
        .filter(|text| !text.is_empty())
    {
        return Some(Provider::Fixture(Arc::from(text)));
    }
    let settings = &all_language_settings(None, cx).edit_predictions;
    match settings.provider {
        EditPredictionProvider::None | EditPredictionProvider::Codestral => None,
        EditPredictionProvider::Copilot => Some(Provider::Copilot),
        EditPredictionProvider::Zed => Some(Provider::Zed(EditPredictionModel::Zeta)),
        EditPredictionProvider::Mercury => Some(Provider::Zed(EditPredictionModel::Mercury)),
        EditPredictionProvider::Ollama | EditPredictionProvider::OpenAiCompatibleApi => {
            let custom = if settings.provider == EditPredictionProvider::Ollama {
                settings.ollama.as_ref()?
            } else {
                settings.open_ai_compatible_api.as_ref()?
            };
            let format = match custom.prompt_format {
                EditPredictionPromptFormat::Infer => infer_prompt_format(&custom.model)?,
                format => format,
            };
            Some(Provider::Zed(match format {
                EditPredictionPromptFormat::Zeta(_) => EditPredictionModel::Zeta,
                EditPredictionPromptFormat::Sweep => EditPredictionModel::SweepPrompt,
                format => EditPredictionModel::Fim { format },
            }))
        }
    }
}

/// The prompt format a local model name implies, as Zed infers it.
fn infer_prompt_format(model: &str) -> Option<EditPredictionPromptFormat> {
    let base = model.split(':').next().unwrap_or(model);
    Some(match base {
        "zeta2" => EditPredictionPromptFormat::Zeta(ZetaVersion::Zeta2),
        "zeta2.1" => EditPredictionPromptFormat::Zeta(ZetaVersion::Zeta2_1),
        model if model.to_ascii_lowercase().contains("sweep-next-edit") => {
            EditPredictionPromptFormat::Sweep
        }
        "codellama" | "code-llama" => EditPredictionPromptFormat::CodeLlama,
        "starcoder" | "starcoder2" | "starcoderbase" => EditPredictionPromptFormat::StarCoder,
        "deepseek-coder" | "deepseek-coder-v2" => EditPredictionPromptFormat::DeepseekCoder,
        "qwen2.5-coder" | "qwen-coder" | "qwen" => EditPredictionPromptFormat::Qwen,
        "codegemma" => EditPredictionPromptFormat::CodeGemma,
        "codestral" | "mistral" => EditPredictionPromptFormat::Codestral,
        "glm" | "glm-4" | "glm-4.5" => EditPredictionPromptFormat::Glm,
        _ => return None,
    })
}

fn assign(
    editor: &mut Editor,
    provider: Option<Provider>,
    trigger: EditPredictionRequestTrigger,
    client: &Arc<Client>,
    user_store: Entity<UserStore>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let buffer = editor.buffer().read(cx).as_singleton();
    match provider {
        None => clear(editor, trigger, window, cx),
        Some(Provider::Fixture(text)) => {
            let provider = cx.new(|_| FixtureDelegate::new(text));
            editor.set_edit_prediction_provider(Some(provider), trigger, window, cx);
            editor.set_show_edit_predictions(Some(true), window, cx);
        }
        Some(Provider::Copilot) => {
            let Some(project) = editor.project().cloned() else {
                return clear(editor, trigger, window, cx);
            };
            let store = edit_prediction::EditPredictionStore::global(client, &user_store, cx);
            let copilot = store.update(cx, |store, cx| {
                store.start_copilot_for_project(&project, cx)
            });
            let Some(copilot) = copilot else {
                return clear(editor, trigger, window, cx);
            };
            if let Some(buffer) = buffer {
                copilot.update(cx, |copilot, cx| copilot.register_buffer(&buffer, cx));
            }
            let provider = cx.new(|_| CopilotEditPredictionDelegate::new(copilot));
            editor.set_edit_prediction_provider(Some(provider), trigger, window, cx);
        }
        Some(Provider::Zed(model)) => {
            let disabled_by_organization = user_store
                .read(cx)
                .current_organization_configuration()
                .is_some_and(|configuration| !configuration.edit_prediction.is_enabled);
            let Some(project) = editor
                .project()
                .cloned()
                .filter(|_| !disabled_by_organization)
            else {
                return clear(editor, trigger, window, cx);
            };
            let store = edit_prediction::EditPredictionStore::global(client, &user_store, cx);
            store.update(cx, |store, cx| {
                store.set_edit_prediction_model(model);
                if let Some(buffer) = &buffer {
                    store.register_buffer(buffer, &project, cx);
                }
            });
            let provider = cx
                .new(|cx| ZedEditPredictionDelegate::new(project, buffer, client, &user_store, cx));
            editor.set_edit_prediction_provider(Some(provider), trigger, window, cx);
        }
    }
}

fn clear(
    editor: &mut Editor,
    trigger: EditPredictionRequestTrigger,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    editor.set_edit_prediction_provider::<ZedEditPredictionDelegate>(None, trigger, window, cx);
}

/// One local insertion at the caret, offered again after every accept or
/// discard.
struct FixtureDelegate {
    text: Arc<str>,
    available: bool,
}

impl FixtureDelegate {
    fn new(text: Arc<str>) -> Self {
        Self {
            text,
            available: true,
        }
    }
}

impl EditPredictionDelegate for FixtureDelegate {
    fn name() -> &'static str {
        "zec-fixture"
    }

    fn display_name() -> &'static str {
        "zec deterministic fixture"
    }

    fn show_predictions_in_menu() -> bool {
        false
    }

    fn show_tab_accept_marker() -> bool {
        true
    }

    fn supports_jump_to_edit() -> bool {
        false
    }

    fn icons(&self, _cx: &App) -> EditPredictionIconSet {
        EditPredictionIconSet::new(IconName::ZedPredict)
    }

    fn is_enabled(&self, _buffer: &Entity<Buffer>, _cursor: language::Anchor, _cx: &App) -> bool {
        true
    }

    fn is_refreshing(&self, _cx: &App) -> bool {
        false
    }

    fn refresh(
        &mut self,
        _buffer: Entity<Buffer>,
        _cursor: language::Anchor,
        _debounce: bool,
        _trigger: EditPredictionRequestTrigger,
        cx: &mut Context<Self>,
    ) {
        if !self.available {
            self.available = true;
            cx.notify();
        }
    }

    fn accept(&mut self, _cx: &mut Context<Self>) {
        self.available = false;
    }

    fn discard(&mut self, _reason: EditPredictionDiscardReason, _cx: &mut Context<Self>) {
        self.available = false;
    }

    fn suggest(
        &mut self,
        _buffer: &Entity<Buffer>,
        cursor: language::Anchor,
        _cx: &mut Context<Self>,
    ) -> Option<EditPrediction> {
        self.available.then(|| EditPrediction::Local {
            id: Some("zec-fixture".into()),
            edits: vec![(cursor..cursor, self.text.clone())],
            cursor_position: None,
            edit_preview: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_prompt_formats_from_model_names() {
        assert!(matches!(
            infer_prompt_format("zeta2.1:latest"),
            Some(EditPredictionPromptFormat::Zeta(ZetaVersion::Zeta2_1))
        ));
        assert_eq!(
            infer_prompt_format("qwen2.5-coder:7b"),
            Some(EditPredictionPromptFormat::Qwen)
        );
        assert_eq!(infer_prompt_format("unknown-model"), None);
    }
}
