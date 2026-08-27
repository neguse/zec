use std::{cell::RefCell, env, rc::Rc, sync::Arc};

use client::{Client, UserStore};
use codestral::{CodestralEditPredictionDelegate, load_codestral_api_key};
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

/// Installs Zed's edit-prediction registry for zec's hidden Editor windows.
///
/// Providers are selected from the same language settings as Zed and are
/// reassigned when settings or organization policy changes. A deterministic
/// fixture can override the configured provider for PTY acceptance tests; it
/// still travels through Editor's real ghost-text and transaction machinery.
pub fn init(client: Arc<Client>, user_store: Entity<UserStore>, cx: &mut App) {
    edit_prediction::EditPredictionStore::global(&client, &user_store, cx);

    let editors: Rc<RefCell<HashMap<WeakEntity<Editor>, AnyWindowHandle>>> = Rc::default();
    cx.observe_new({
        let editors = editors.clone();
        let client = client.clone();
        let user_store = user_store.clone();
        move |editor: &mut Editor, window, cx: &mut Context<Editor>| {
            if !editor.mode().is_full() {
                return;
            }
            let Some(window) = window else {
                return;
            };

            let editor_handle = cx.entity().downgrade();
            cx.on_release({
                let editor_handle = editor_handle.clone();
                let editors = editors.clone();
                move |_, _| {
                    editors.borrow_mut().remove(&editor_handle);
                }
            })
            .detach();
            editors
                .borrow_mut()
                .insert(editor_handle, window.window_handle());

            assign_edit_prediction_provider(
                editor,
                provider_config_for_settings(cx),
                EditPredictionRequestTrigger::EditorCreated,
                &client,
                user_store.clone(),
                window,
                cx,
            );
        }
    })
    .detach();

    cx.on_action(clear_edit_prediction_store_edit_history);

    cx.subscribe(&user_store, {
        let editors = editors.clone();
        let client = client.clone();
        move |user_store, event, cx| match event {
            client::user::Event::PrivateUserInfoUpdated
            | client::user::Event::OrganizationChanged => assign_edit_prediction_providers(
                &editors,
                provider_config_for_settings(cx),
                EditPredictionRequestTrigger::UserInfoChanged,
                &client,
                user_store,
                cx,
            ),
            _ => {}
        }
    })
    .detach();

    cx.observe_global::<SettingsStore>({
        let mut previous_config = provider_config_for_settings(cx);
        move |cx| {
            let new_config = provider_config_for_settings(cx);
            if new_config != previous_config {
                previous_config = new_config.clone();
                assign_edit_prediction_providers(
                    &editors,
                    new_config,
                    EditPredictionRequestTrigger::ProviderChanged,
                    &client,
                    user_store.clone(),
                    cx,
                );
            }
        }
    })
    .detach();
}

fn provider_config_for_settings(cx: &App) -> Option<EditPredictionProviderConfig> {
    if let Some(text) = fixture_text() {
        return Some(EditPredictionProviderConfig::Fixture(text));
    }

    let settings = &all_language_settings(None, cx).edit_predictions;
    let provider = settings.provider;
    match provider {
        EditPredictionProvider::None => None,
        EditPredictionProvider::Copilot => Some(EditPredictionProviderConfig::Copilot),
        EditPredictionProvider::Codestral => Some(EditPredictionProviderConfig::Codestral),
        EditPredictionProvider::Zed => {
            Some(EditPredictionProviderConfig::Zed(EditPredictionModel::Zeta))
        }
        EditPredictionProvider::Mercury => Some(EditPredictionProviderConfig::Zed(
            EditPredictionModel::Mercury,
        )),
        EditPredictionProvider::Ollama | EditPredictionProvider::OpenAiCompatibleApi => {
            let custom_settings = if provider == EditPredictionProvider::Ollama {
                settings.ollama.as_ref()?
            } else {
                settings.open_ai_compatible_api.as_ref()?
            };
            let mut format = custom_settings.prompt_format;
            if format == EditPredictionPromptFormat::Infer {
                format = infer_prompt_format(&custom_settings.model)?;
            }

            if matches!(format, EditPredictionPromptFormat::Zeta(_)) {
                Some(EditPredictionProviderConfig::Zed(EditPredictionModel::Zeta))
            } else if format == EditPredictionPromptFormat::Sweep {
                Some(EditPredictionProviderConfig::Zed(
                    EditPredictionModel::SweepPrompt,
                ))
            } else {
                Some(EditPredictionProviderConfig::Zed(
                    EditPredictionModel::Fim { format },
                ))
            }
        }
    }
}

fn infer_prompt_format(model: &str) -> Option<EditPredictionPromptFormat> {
    let model_base = model.split(':').next().unwrap_or(model);
    Some(match model_base {
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

#[derive(Clone, PartialEq, Eq)]
enum EditPredictionProviderConfig {
    Fixture(Arc<str>),
    Copilot,
    Codestral,
    Zed(EditPredictionModel),
}

fn clear_edit_prediction_store_edit_history(_: &edit_prediction::ClearHistory, cx: &mut App) {
    if let Some(store) = edit_prediction::EditPredictionStore::try_global(cx) {
        store.update(cx, |store, _| store.clear_history());
    }
}

fn assign_edit_prediction_providers(
    editors: &Rc<RefCell<HashMap<WeakEntity<Editor>, AnyWindowHandle>>>,
    provider_config: Option<EditPredictionProviderConfig>,
    trigger: EditPredictionRequestTrigger,
    client: &Arc<Client>,
    user_store: Entity<UserStore>,
    cx: &mut App,
) {
    if provider_config == Some(EditPredictionProviderConfig::Codestral) {
        load_codestral_api_key(cx).detach();
    }
    for (editor, window) in editors.borrow().iter() {
        _ = window.update(cx, |_root, window, cx| {
            _ = editor.update(cx, |editor, cx| {
                assign_edit_prediction_provider(
                    editor,
                    provider_config.clone(),
                    trigger,
                    client,
                    user_store.clone(),
                    window,
                    cx,
                );
            });
        });
    }
}

fn assign_edit_prediction_provider(
    editor: &mut Editor,
    provider_config: Option<EditPredictionProviderConfig>,
    trigger: EditPredictionRequestTrigger,
    client: &Arc<Client>,
    user_store: Entity<UserStore>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let singleton_buffer = editor.buffer().read(cx).as_singleton();
    match provider_config {
        None => clear_provider(editor, trigger, window, cx),
        Some(EditPredictionProviderConfig::Fixture(text)) => {
            let provider = cx.new(|_| FixtureEditPredictionDelegate::new(text));
            editor.set_edit_prediction_provider(Some(provider), trigger, window, cx);
            editor.set_show_edit_predictions(Some(true), window, cx);
        }
        Some(EditPredictionProviderConfig::Copilot) => {
            let Some(project) = editor.project().cloned() else {
                clear_provider(editor, trigger, window, cx);
                return;
            };
            let store = edit_prediction::EditPredictionStore::global(client, &user_store, cx);
            let copilot = store.update(cx, |store, cx| {
                store.start_copilot_for_project(&project, cx)
            });
            let Some(copilot) = copilot else {
                clear_provider(editor, trigger, window, cx);
                return;
            };
            if let Some(buffer) = singleton_buffer {
                copilot.update(cx, |copilot, cx| copilot.register_buffer(&buffer, cx));
            }
            let provider = cx.new(|_| CopilotEditPredictionDelegate::new(copilot));
            editor.set_edit_prediction_provider(Some(provider), trigger, window, cx);
        }
        Some(EditPredictionProviderConfig::Codestral) => {
            let provider = cx.new(|_| CodestralEditPredictionDelegate::new(client.http_client()));
            editor.set_edit_prediction_provider(Some(provider), trigger, window, cx);
        }
        Some(EditPredictionProviderConfig::Zed(model)) => {
            if user_store
                .read(cx)
                .current_organization_configuration()
                .is_some_and(|configuration| !configuration.edit_prediction.is_enabled)
            {
                clear_provider(editor, trigger, window, cx);
                return;
            }
            let Some(project) = editor.project().cloned() else {
                clear_provider(editor, trigger, window, cx);
                return;
            };
            let store = edit_prediction::EditPredictionStore::global(client, &user_store, cx);
            store.update(cx, |store, cx| {
                store.set_edit_prediction_model(model);
                if let Some(buffer) = &singleton_buffer {
                    store.register_buffer(buffer, &project, cx);
                }
            });
            let provider = cx.new(|cx| {
                ZedEditPredictionDelegate::new(project, singleton_buffer, client, &user_store, cx)
            });
            editor.set_edit_prediction_provider(Some(provider), trigger, window, cx);
        }
    }
}

fn clear_provider(
    editor: &mut Editor,
    trigger: EditPredictionRequestTrigger,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    editor.set_edit_prediction_provider::<ZedEditPredictionDelegate>(None, trigger, window, cx);
}

fn fixture_text() -> Option<Arc<str>> {
    env::var("ZEC_EDIT_PREDICTION_FIXTURE")
        .ok()
        .filter(|text| !text.is_empty())
        .map(Arc::from)
}

struct FixtureEditPredictionDelegate {
    text: Arc<str>,
    available: bool,
}

impl FixtureEditPredictionDelegate {
    fn new(text: Arc<str>) -> Self {
        Self {
            text,
            available: true,
        }
    }
}

impl EditPredictionDelegate for FixtureEditPredictionDelegate {
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

    fn is_enabled(
        &self,
        _buffer: &Entity<Buffer>,
        _cursor_position: language::Anchor,
        _cx: &App,
    ) -> bool {
        true
    }

    fn is_refreshing(&self, _cx: &App) -> bool {
        false
    }

    fn refresh(
        &mut self,
        _buffer: Entity<Buffer>,
        _cursor_position: language::Anchor,
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
        cursor_position: language::Anchor,
        _cx: &mut Context<Self>,
    ) -> Option<EditPrediction> {
        self.available.then(|| EditPrediction::Local {
            id: Some("zec-fixture".into()),
            edits: vec![(cursor_position..cursor_position, self.text.clone())],
            cursor_position: None,
            edit_preview: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_supported_custom_provider_prompt_formats() {
        assert!(matches!(
            infer_prompt_format("zeta2.1:latest"),
            Some(EditPredictionPromptFormat::Zeta(ZetaVersion::Zeta2_1))
        ));
        assert_eq!(
            infer_prompt_format("sweep-next-edit-1.5b"),
            Some(EditPredictionPromptFormat::Sweep)
        );
        assert_eq!(
            infer_prompt_format("qwen2.5-coder:7b"),
            Some(EditPredictionPromptFormat::Qwen)
        );
        assert_eq!(infer_prompt_format("unknown-model"), None);
    }
}
