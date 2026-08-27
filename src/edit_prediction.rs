use std::{env, sync::Arc};

use client::{Client, UserStore};
use edit_prediction_types::{
    EditPrediction, EditPredictionDelegate, EditPredictionDiscardReason, EditPredictionIconSet,
    EditPredictionRequestTrigger,
};
use editor::Editor;
use gpui::{App, AppContext as _, Context, Entity, Window};
use icons::IconName;
use language::Buffer;
use project::Project;

/// Installs the same Zed Predict delegate used by Zed's GUI on a hidden editor.
///
/// A deterministic provider is selected only when the fixture variable is set.
/// It deliberately still travels through Editor's edit-prediction machinery so
/// PTY tests cover DisplayMap ghost text and Editor transactions, not a terminal-
/// only imitation.
pub fn install(
    editor: &mut Editor,
    buffer: Entity<Buffer>,
    project: Entity<Project>,
    client: &Arc<Client>,
    user_store: &Entity<UserStore>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    if let Some(text) = fixture_text() {
        let provider = cx.new(|_| FixtureEditPredictionDelegate::new(text));
        editor.set_edit_prediction_provider(
            Some(provider),
            EditPredictionRequestTrigger::EditorCreated,
            window,
            cx,
        );
        // Tests must not depend on the user's global `show_edit_predictions`
        // toggle. The override is scoped to this Editor and can still be
        // changed with ToggleEditPrediction.
        editor.set_show_edit_predictions(Some(true), window, cx);
        return;
    }

    let store = edit_prediction::EditPredictionStore::global(client, user_store, cx);
    store.update(cx, |store, cx| {
        store.register_buffer(&buffer, &project, cx);
    });
    let provider = cx.new(|cx| {
        edit_prediction::ZedEditPredictionDelegate::new(
            project,
            Some(buffer),
            client,
            user_store,
            cx,
        )
    });
    editor.set_edit_prediction_provider(
        Some(provider),
        EditPredictionRequestTrigger::EditorCreated,
        window,
        cx,
    );
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
            // The completion is already available when Editor installs the
            // delegate, so its initial update_visible_edit_prediction call can
            // consume it synchronously without an observer notification loop.
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
