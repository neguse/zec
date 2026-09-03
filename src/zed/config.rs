//! Settings and keymap file watchers.

use std::sync::Arc;

use async_channel::Sender;
use futures::StreamExt as _;
use gpui::{App, UpdateGlobal as _};
use zed_fs::Fs;

use super::{Event, keymap};

/// Reloads settings and the user keymap when their files change, reporting
/// each reload as an event. An invalid keymap keeps the last valid bindings.
pub fn start_watchers<T>(
    fs: Arc<dyn Fs>,
    zec_keymap: &'static str,
    sender: Sender<T>,
    on_keymap: impl Fn(keymap::Lookup) + 'static,
    cx: &mut App,
) where
    T: From<Event> + Send + 'static,
{
    let settings_sender = sender.clone();
    settings::SettingsStore::update_global(cx, {
        let fs = fs.clone();
        move |store, cx| {
            store.watch_settings_files(fs, cx, move |settings_file, result, _cx| {
                let kind = match settings_file {
                    settings::SettingsFile::User => "user settings",
                    settings::SettingsFile::Global => "global settings",
                    _ => "settings",
                };
                let result = result
                    .result()
                    .map(|migrated| {
                        if migrated {
                            "reloaded (migration applied in memory)".to_owned()
                        } else {
                            "reloaded".to_owned()
                        }
                    })
                    .map_err(|error| format!("{error:#}"));
                let _ = settings_sender.try_send(Event::ConfigReloaded { kind, result }.into());
            });
        }
    });

    let (mut keymap_rx, keymap_watcher) =
        settings::watch_config_file(cx.background_executor(), fs, paths::keymap_file().clone());
    cx.spawn(async move |cx| {
        let _keymap_watcher = keymap_watcher;
        let mut last_good = Vec::new();
        while let Some(content) = keymap_rx.next().await {
            let result = cx.update(|cx| {
                let (bindings, outcome) = match keymap::load(&content, cx) {
                    Ok(bindings) => (bindings, Ok("reloaded".to_owned())),
                    Err(error) => (
                        last_good.clone(),
                        Err(format!(
                            "invalid keymap; retained last valid bindings: {error}"
                        )),
                    ),
                };
                match keymap::apply(zec_keymap, bindings.clone(), cx) {
                    Ok(lookup) => {
                        on_keymap(lookup);
                        if outcome.is_ok() {
                            last_good = bindings;
                        }
                        outcome
                    }
                    Err(error) => Err(format!("could not apply keymap: {error:#}")),
                }
            });
            let _ = sender.try_send(
                Event::ConfigReloaded {
                    kind: "user keymap",
                    result,
                }
                .into(),
            );
        }
    })
    .detach();
}
