//! Documents: one Zed Buffer shown through one hidden Editor per item.
//!
//! Label, dirty state, and disk state are read from the Buffer's file at
//! use time and never cached, so an external rename or delete is reflected
//! the moment Zed sees it. A split shows one Buffer through two Editors;
//! the Buffer itself is watched once, for as long as any item shows it.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result};
use async_channel::Sender;
use editor::Editor;
use gpui::{AsyncApp, Entity, Subscription, WindowHandle};
use language::Buffer;

use crate::{
    app::{event::Event, workspace::ItemId},
    terminal::render::{Follow, Viewport},
    zed::{
        self,
        services::{BufferState, Services, buffer_state},
    },
};

pub struct Document {
    pub buffer: Entity<Buffer>,
    pub editor: WindowHandle<Editor>,
    /// Terminal-side presentation only; text, selection, and undo are Zed's.
    pub viewport: Viewport,
    pub follow: Follow,
    untitled: Option<String>,
}

impl Document {
    /// Opens a hidden window for `buffer` and forwards its repaints.
    fn view(
        buffer: Entity<Buffer>,
        untitled: Option<String>,
        services: &Services,
        events: &Sender<Event>,
        cx: &mut AsyncApp,
    ) -> Result<Self> {
        let project = services.project.clone();
        let editor = cx.update(|cx| zed::editor::open_window(buffer.clone(), Some(project), cx))?;
        if let Err(error) = zed::editor::observe_editor(&editor, events.clone(), cx) {
            let _ = zed::editor::close_window(&editor, cx);
            return Err(error);
        }
        Ok(Self {
            buffer,
            editor,
            viewport: Viewport::default(),
            follow: Follow::default(),
            untitled,
        })
    }

    pub fn state(&self, cx: &AsyncApp) -> BufferState {
        buffer_state(&self.buffer, cx)
    }

    pub fn label(&self, cx: &AsyncApp) -> String {
        match self.state(cx).path {
            Some(path) => path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            None => self
                .untitled
                .clone()
                .unwrap_or_else(|| "[No Name]".to_owned()),
        }
    }

    pub fn buffer_id(&self, cx: &AsyncApp) -> u64 {
        zed::services::buffer_id(&self.buffer, cx)
    }
}

/// One watcher per Buffer: external changes reload it once, however many
/// items show it.
struct Watcher {
    items: usize,
    _subscription: Subscription,
}

#[derive(Default)]
pub struct Documents {
    by_item: BTreeMap<ItemId, Document>,
    watchers: BTreeMap<u64, Watcher>,
    next_item: u64,
    next_untitled: usize,
}

impl Documents {
    /// Opens `buffer` as a new item.
    pub fn open(
        &mut self,
        buffer: Entity<Buffer>,
        untitled: Option<String>,
        services: &Services,
        events: &Sender<Event>,
        cx: &mut AsyncApp,
    ) -> Result<ItemId> {
        let document = Document::view(buffer, untitled, services, events, cx)?;
        self.insert(document, services, events, cx)
    }

    /// Opens a second item on the Buffer of `item`, for a split.
    pub fn split(
        &mut self,
        item: ItemId,
        services: &Services,
        events: &Sender<Event>,
        cx: &mut AsyncApp,
    ) -> Result<ItemId> {
        let source = self.get(item).context("split source has no document")?;
        let document = Document::view(
            source.buffer.clone(),
            source.untitled.clone(),
            services,
            events,
            cx,
        )?;
        self.insert(document, services, events, cx)
    }

    fn insert(
        &mut self,
        document: Document,
        services: &Services,
        events: &Sender<Event>,
        cx: &mut AsyncApp,
    ) -> Result<ItemId> {
        let buffer_id = document.buffer_id(cx);
        match self.watchers.get_mut(&buffer_id) {
            Some(watcher) => watcher.items += 1,
            None => {
                let subscription = zed::editor::watch_buffer(
                    &document.buffer,
                    services.buffer_store.clone(),
                    events.clone(),
                    cx,
                );
                self.watchers.insert(
                    buffer_id,
                    Watcher {
                        items: 1,
                        _subscription: subscription,
                    },
                );
            }
        }
        self.next_item += 1;
        let item = ItemId(self.next_item);
        self.by_item.insert(item, document);
        Ok(item)
    }

    pub fn get(&self, item: ItemId) -> Option<&Document> {
        self.by_item.get(&item)
    }

    pub fn get_mut(&mut self, item: ItemId) -> Option<&mut Document> {
        self.by_item.get_mut(&item)
    }

    /// Closes the item's window; the Buffer stops being watched with its
    /// last item.
    pub fn close(&mut self, item: ItemId, cx: &mut AsyncApp) -> Result<()> {
        let document = self
            .by_item
            .remove(&item)
            .context("closing item has no document")?;
        let buffer_id = document.buffer_id(cx);
        if let Some(watcher) = self.watchers.get_mut(&buffer_id) {
            watcher.items -= 1;
            if watcher.items == 0 {
                self.watchers.remove(&buffer_id);
            }
        }
        zed::editor::close_window(&document.editor, cx)
    }

    pub fn close_all(&mut self, cx: &mut AsyncApp) {
        for item in self.by_item.keys().copied().collect::<Vec<_>>() {
            let _ = self.close(item, cx);
        }
    }

    pub fn len(&self) -> usize {
        self.by_item.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (ItemId, &Document)> {
        self.by_item
            .iter()
            .map(|(item, document)| (*item, document))
    }

    /// Every item showing `buffer`, so an open never duplicates a tab.
    pub fn items_for_buffer(&self, buffer: &Entity<Buffer>) -> Vec<ItemId> {
        self.iter()
            .filter(|(_, document)| document.buffer == *buffer)
            .map(|(item, _)| item)
            .collect()
    }

    pub fn shows_buffer_id(&self, buffer_id: u64, cx: &AsyncApp) -> bool {
        self.iter()
            .any(|(_, document)| document.buffer_id(cx) == buffer_id)
    }

    /// Process-monotonic scratch labels, so a closed `Untitled 1` is never
    /// reused while another tab could still refer to it.
    pub fn next_untitled_label(&mut self) -> String {
        self.next_untitled += 1;
        format!("Untitled {}", self.next_untitled)
    }
}
