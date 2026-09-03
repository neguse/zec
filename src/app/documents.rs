//! Documents: one Zed Buffer shown through one hidden Editor per item.
//!
//! Label, dirty state, and disk state are read from the Buffer's file at
//! use time and never cached, so an external rename or delete is reflected
//! the moment Zed sees it.

use std::collections::BTreeMap;

use anyhow::Result;
use async_channel::Sender;
use editor::Editor;
use gpui::{AsyncApp, Entity, WindowHandle};
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
    /// Opens the hidden window for `buffer` and starts forwarding its
    /// changes as events.
    pub fn open(
        buffer: Entity<Buffer>,
        untitled: Option<String>,
        services: &Services,
        events: &Sender<Event>,
        cx: &mut AsyncApp,
    ) -> Result<Self> {
        let project = services.project.clone();
        let editor = cx.update(|cx| zed::editor::open_window(buffer.clone(), Some(project), cx))?;
        if let Err(error) = zed::editor::subscribe(
            &editor,
            &buffer,
            services.buffer_store.clone(),
            events.clone(),
            cx,
        ) {
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

#[derive(Default)]
pub struct Documents {
    by_item: BTreeMap<ItemId, Document>,
    next_item: u64,
    next_untitled: usize,
}

impl Documents {
    pub fn insert(&mut self, document: Document) -> ItemId {
        self.next_item += 1;
        let item = ItemId(self.next_item);
        self.by_item.insert(item, document);
        item
    }

    pub fn get(&self, item: ItemId) -> Option<&Document> {
        self.by_item.get(&item)
    }

    pub fn get_mut(&mut self, item: ItemId) -> Option<&mut Document> {
        self.by_item.get_mut(&item)
    }

    pub fn remove(&mut self, item: ItemId) -> Option<Document> {
        self.by_item.remove(&item)
    }

    pub fn len(&self) -> usize {
        self.by_item.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (ItemId, &Document)> {
        self.by_item
            .iter()
            .map(|(item, document)| (*item, document))
    }

    /// The item already showing `buffer`, so an open never duplicates a tab.
    pub fn item_for_buffer(&self, buffer: &Entity<Buffer>) -> Option<ItemId> {
        self.iter()
            .find(|(_, document)| document.buffer == *buffer)
            .map(|(item, _)| item)
    }

    pub fn item_for_buffer_id(&self, buffer_id: u64, cx: &AsyncApp) -> Option<ItemId> {
        self.iter()
            .find(|(_, document)| document.buffer_id(cx) == buffer_id)
            .map(|(item, _)| item)
    }

    /// Process-monotonic scratch labels, so a closed `Untitled 1` is never
    /// reused while another tab could still refer to it.
    pub fn next_untitled_label(&mut self) -> String {
        self.next_untitled += 1;
        format!("Untitled {}", self.next_untitled)
    }
}
