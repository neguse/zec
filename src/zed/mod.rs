//! The Zed boundary: GPUI boot, the local Project and its stores, hidden
//! Editor windows, snapshot capture, and the keymap. Nothing here knows about
//! the application state; changes cross back as [`Event`]s.

pub mod config;
pub mod editor;
pub mod keymap;
pub mod runtime;
pub mod services;

/// A change on the Zed side that the application must see.
#[derive(Debug)]
pub enum Event {
    /// Something visible changed; the next frame reads the new snapshot.
    Redraw,
    /// An external change finished reloading into a clean buffer.
    ReloadFinished {
        buffer_id: u64,
        result: Result<(), String>,
    },
    /// A settings or keymap file was reloaded.
    ConfigReloaded {
        kind: &'static str,
        result: Result<String, String>,
    },
    /// Zed refused to start repository-controlled processes in a worktree
    /// until it is trusted.
    WorktreeRestricted { path: std::path::PathBuf },
    /// A language server started or stopped.
    LanguageServer { name: String, running: bool },
}

/// Headless proof that the pinned Zed Editor runs: insert, print, undo.
pub fn smoke() {
    use ::editor::actions::Undo;
    use gpui::AppContext as _;
    use language::Buffer;

    runtime::application().run(|cx| {
        runtime::init(cx);
        let buffer = cx.new(|cx| Buffer::local(String::new(), cx));
        let window = editor::open_window(buffer, None, cx).expect("failed to open editor");

        cx.spawn(async move |cx| {
            window
                .update(cx, |editor, window, cx| {
                    println!("initial: {:?}", editor.text(cx));
                    editor.insert("hello from zec", window, cx);
                    println!("after insert: {:?}", editor.text(cx));
                    editor.undo(&Undo, window, cx);
                    println!("after undo: {:?}", editor.text(cx));
                })
                .expect("failed to update editor");
            cx.update(|cx| cx.quit());
        })
        .detach();
    });
}
