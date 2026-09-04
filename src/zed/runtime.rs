//! GPUI application boot and the process-wide Zed globals.

use std::sync::Arc;

use client::{Client, UserStore};
use gpui::{App, AppContext as _, Entity, Global};
use language::LanguageRegistry;
use node_runtime::{NodeBinaryOptions, NodeRuntime};
use project::Project;
use theme::ActiveTheme as _;
use zed_fs::{Fs, RealFs};

/// Services shared by every Project in the process.
#[derive(Clone)]
pub struct Runtime {
    pub client: Arc<Client>,
    pub user_store: Entity<UserStore>,
    pub node_runtime: NodeRuntime,
    pub fs: Arc<dyn Fs>,
    pub languages: Arc<LanguageRegistry>,
}

impl Global for Runtime {}

pub fn application() -> gpui::Application {
    #[cfg(windows)]
    {
        // The pinned GPUI Windows headless platform omits the devices that
        // `open_window` needs. zec keeps its editor windows hidden but still
        // needs the regular platform to construct Zed's Editor.
        gpui_platform::application()
    }
    #[cfg(not(windows))]
    {
        gpui_platform::headless()
    }
}

/// Initializes Zed's settings, theme, editor, and project services once.
pub fn init(cx: &mut App) {
    release_channel::init_test(
        semver::Version::parse(env!("CARGO_PKG_VERSION"))
            .expect("zec package version must be valid semver"),
        release_channel::ReleaseChannel::Stable,
        cx,
    );
    // Zed's app database, under the data directory; the edit prediction
    // store and other Zed services keep their state there.
    cx.set_global(db::AppDatabase::new());
    gpui_tokio::init(cx);
    settings::init(cx);
    theme_settings::init(theme::LoadThemes::All(Box::new(assets::Assets)), cx);
    editor::init(cx);
    project::trusted_worktrees::init(Default::default(), cx);

    if cx.has_global::<Runtime>() {
        return;
    }

    let client = Client::production(cx);
    Client::set_global(client.clone(), cx);
    Project::init(&client, cx);
    let user_store = cx.new(|cx| UserStore::new(client.clone(), cx));
    client::init(&client, cx);
    let (mut node_options_tx, node_options_rx) = watch::channel(None);
    let _ = node_options_tx.send(Some(NodeBinaryOptions {
        allow_path_lookup: true,
        allow_binary_download: false,
        use_paths: None,
    }));
    let node_runtime = NodeRuntime::new(client.http_client(), None, node_options_rx);
    let fs: Arc<dyn Fs> = Arc::new(RealFs::new(None, cx.background_executor().clone()));
    <dyn Fs>::set_global(fs.clone(), cx);
    let languages = Arc::new(LanguageRegistry::new(cx.background_executor().clone()));
    languages.set_theme(cx.theme().clone());
    // Native grammars register as lazy loaders; queries compile on first use.
    languages::init(languages.clone(), fs.clone(), node_runtime.clone(), cx);

    cx.set_global(Runtime {
        client,
        user_store,
        node_runtime,
        fs,
        languages,
    });
}
