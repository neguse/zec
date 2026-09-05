//! Keymap loading and lookup.
//!
//! Zed's default keymap, zec's default keymap, and the user's `keymap.json`
//! are bound into GPUI in that order so later sources win. The same bindings
//! are kept in a [`Lookup`] so owners without a window, such as prompts, can
//! resolve keys in their own key context.

use anyhow::{Context as _, Result};
use gpui::{App, KeyBinding, KeyContext, Keymap, Keystroke};
use settings::{KeymapFile, KeymapFileLoadResult};

pub struct Lookup {
    keymap: Keymap,
}

impl Lookup {
    /// Action names bound to `keystroke` in `context`, strongest first.
    pub fn resolve(&self, keystroke: &Keystroke, context: &str) -> Vec<&'static str> {
        let Ok(context) = KeyContext::parse(context) else {
            return Vec::new();
        };
        let (bindings, _pending) = self
            .keymap
            .bindings_for_input(std::slice::from_ref(keystroke), &[context]);
        bindings
            .iter()
            .map(|binding| binding.action().name())
            .collect()
    }

    /// The last single-keystroke binding for an action, which is the one
    /// from the strongest source.
    pub fn keystroke_for(&self, action_name: &str) -> Option<Keystroke> {
        self.keymap
            .bindings()
            .rev()
            .find(|binding| {
                binding.action().name() == action_name && binding.keystrokes().len() == 1
            })
            .map(|binding| binding.keystrokes()[0].inner().clone())
    }
}

/// Binds every source into GPUI and returns the combined lookup.
pub fn apply(zec_defaults: &str, user_bindings: Vec<KeyBinding>, cx: &mut App) -> Result<Lookup> {
    let defaults = KeymapFile::load_asset_allow_partial_failure(settings::DEFAULT_KEYMAP_PATH, cx)
        .context("load Zed default keymap")?;
    let zec = load(zec_defaults, cx)
        .map_err(anyhow::Error::msg)
        .context("load zec default keymap")?;
    cx.clear_key_bindings();
    cx.bind_keys(defaults.clone());
    cx.bind_keys(zec.clone());
    cx.bind_keys(user_bindings.clone());
    let mut all = defaults;
    all.extend(zec);
    all.extend(user_bindings);
    Ok(Lookup {
        keymap: Keymap::new(all),
    })
}

/// Parses keymap JSON. Partially valid files keep their valid bindings and
/// report the rest.
pub fn load(content: &str, cx: &App) -> Result<Vec<KeyBinding>, String> {
    match KeymapFile::load(content, cx) {
        KeymapFileLoadResult::Success { key_bindings } => Ok(key_bindings),
        KeymapFileLoadResult::SomeFailedToLoad {
            key_bindings,
            error_message,
        } if !key_bindings.is_empty() => {
            log::warn!("keymap partially loaded: {error_message}");
            Ok(key_bindings)
        }
        KeymapFileLoadResult::SomeFailedToLoad { error_message, .. } => {
            Err(error_message.to_string())
        }
        KeymapFileLoadResult::JsonParseFailure { error } => Err(format!("{error:#}")),
    }
}
