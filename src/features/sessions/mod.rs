//! Sessions: the layout and open files of a directory root, restored on
//! the next start.
//!
//! One JSON file per root, keyed by the canonical root path, under zec's
//! data directory. The file records the pane tree, each pane's file tabs
//! with their caret and viewport, and the active pane and tab; scratch
//! tabs are not persisted. Files that no longer exist are skipped on
//! restore, and a file zec cannot parse is ignored and overwritten by the
//! next save.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use async_channel::Sender;
use gpui::AsyncApp;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{
    app::{
        documents::Documents,
        event::Event,
        feature::Ctx,
        workspace::{Axis, LayoutNode, Pane, PaneId, WorkspaceModel},
    },
    zed::{self, services::Services},
};

const VERSION: u32 = 1;
const DIRECTORY: &str = "zec-sessions";

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Session {
    pub version: u32,
    pub layout: Node,
    /// Indexed by the `pane` numbers in `layout`.
    pub panes: Vec<PaneSession>,
    pub active_pane: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Node {
    Pane(usize),
    Split {
        axis: SplitAxis,
        ratio: u16,
        first: Box<Node>,
        second: Box<Node>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PaneSession {
    pub tabs: Vec<Tab>,
    pub active: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Tab {
    pub path: PathBuf,
    pub row: u32,
    pub column: u32,
    pub top_row: usize,
}

/// What a restore produced; `workspace` is `None` when no tab came back.
pub struct Restored {
    pub documents: Documents,
    pub workspace: Option<WorkspaceModel>,
    pub restored: usize,
    pub skipped: usize,
}

#[derive(Default)]
pub struct Sessions {
    file: Option<PathBuf>,
    /// The tab-and-layout shape last written, so carets alone do not
    /// cause a write until shutdown.
    written: Option<String>,
}

impl Sessions {
    /// Chooses the session file for `root`.
    pub fn bind(&mut self, root: &Path) {
        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
        let key = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        self.file = Some(
            paths::data_dir()
                .join(DIRECTORY)
                .join(format!("{key}.json")),
        );
    }

    /// The stored session, if there is a readable one.
    pub fn load(&self) -> Option<Session> {
        let file = self.file.as_ref()?;
        let content = match fs::read(file) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
            Err(error) => {
                log::warn!("session {} unreadable: {error}", file.display());
                return None;
            }
        };
        match serde_json::from_slice::<Session>(&content) {
            Ok(session) if session.version == VERSION => Some(session),
            Ok(session) => {
                log::warn!("session {} has version {}", file.display(), session.version);
                None
            }
            Err(error) => {
                log::warn!("session {} ignored: {error}", file.display());
                None
            }
        }
    }

    /// Reopens the session's files and rebuilds its layout. Missing files
    /// are skipped, and a pane left without tabs collapses.
    pub async fn restore(
        &self,
        session: Session,
        services: &Services,
        events: &Sender<Event>,
        cx: &mut AsyncApp,
    ) -> Restored {
        let mut documents = Documents::default();
        let mut panes = BTreeMap::new();
        let mut ids = Vec::with_capacity(session.panes.len());
        let mut restored = 0;
        let mut skipped = 0;
        for (index, pane) in session.panes.iter().enumerate() {
            let mut items = Vec::new();
            let mut active = None;
            for (tab_index, tab) in pane.tabs.iter().enumerate() {
                let is_file = matches!(services.metadata(&tab.path).await, Ok(Some(metadata)) if !metadata.is_dir);
                if !is_file {
                    skipped += 1;
                    continue;
                }
                let opened = match services.open_file(&tab.path, cx).await {
                    Ok(buffer) => documents.open(buffer, None, services, events, cx),
                    Err(error) => Err(error),
                };
                let item = match opened {
                    Ok(item) => item,
                    Err(error) => {
                        log::warn!("session tab {} skipped: {error:#}", tab.path.display());
                        skipped += 1;
                        continue;
                    }
                };
                if let Some(document) = documents.get_mut(item) {
                    document.viewport.top_row = tab.top_row;
                    let _ = zed::editor::place_caret_at_point(
                        &document.editor,
                        text::Point::new(tab.row, tab.column),
                        cx,
                    );
                }
                if tab_index == pane.active {
                    active = Some(items.len());
                }
                items.push(item);
                restored += 1;
            }
            if items.is_empty() {
                ids.push(None);
                continue;
            }
            let id = PaneId(index as u64 + 1);
            let active = active.unwrap_or(items.len() - 1);
            panes.insert(id, Pane::new(items, active));
            ids.push(Some(id));
        }

        let workspace = build(&session.layout, &ids).and_then(|root| {
            let active_pane = ids
                .get(session.active_pane)
                .copied()
                .flatten()
                .unwrap_or_else(|| root.first_pane());
            match WorkspaceModel::from_layout(root, panes, active_pane) {
                Ok(workspace) => Some(workspace),
                Err(error) => {
                    log::warn!("session layout ignored: {error}");
                    None
                }
            }
        });
        if workspace.is_none() {
            documents.close_all(cx);
            return Restored {
                documents: Documents::default(),
                workspace: None,
                restored: 0,
                skipped,
            };
        }
        Restored {
            documents,
            workspace,
            restored,
            skipped,
        }
    }

    /// Writes the session when the tab set or the layout changed.
    pub fn sync(&mut self, ctx: &mut Ctx, cx: &mut AsyncApp) {
        if self.file.is_none() {
            return;
        }
        let session = snapshot(ctx, cx);
        let shape = shape_key(&session);
        if self.written.as_deref() == Some(shape.as_str()) {
            return;
        }
        self.write(&session, shape, ctx);
    }

    /// Writes the exact state, carets and viewports included.
    pub fn finish(&mut self, ctx: &mut Ctx, cx: &mut AsyncApp) {
        if self.file.is_none() {
            return;
        }
        let session = snapshot(ctx, cx);
        let shape = shape_key(&session);
        self.write(&session, shape, ctx);
    }

    fn write(&mut self, session: &Session, shape: String, ctx: &mut Ctx) {
        let Some(file) = self.file.as_ref() else {
            return;
        };
        match write_atomically(file, session) {
            Ok(()) => self.written = Some(shape),
            Err(error) => ctx.status.set(format!("session save failed: {error:#}")),
        }
    }
}

fn snapshot(ctx: &mut Ctx, cx: &mut AsyncApp) -> Session {
    let workspace = ctx.workspace;
    let index_of = workspace
        .panes()
        .enumerate()
        .map(|(index, (id, _))| (id, index))
        .collect::<BTreeMap<_, _>>();
    let panes = workspace
        .panes()
        .map(|(_, pane)| {
            let mut tabs = Vec::new();
            let mut active = 0;
            for (index, item) in pane.items().iter().enumerate() {
                let Some(document) = ctx.documents.get(*item) else {
                    continue;
                };
                let Some(path) = document.state(cx).path else {
                    if index == pane.active_index() {
                        active = tabs.len().saturating_sub(1);
                    }
                    continue;
                };
                if index == pane.active_index() {
                    active = tabs.len();
                }
                let (row, column) = zed::editor::caret_point(&document.editor, cx)
                    .map(|point| (point.row, point.column))
                    .unwrap_or((0, 0));
                tabs.push(Tab {
                    path,
                    row,
                    column,
                    top_row: document.viewport.top_row,
                });
            }
            PaneSession { tabs, active }
        })
        .collect();
    Session {
        version: VERSION,
        layout: node_of(workspace.root(), &index_of),
        panes,
        active_pane: index_of[&workspace.active_pane()],
    }
}

fn node_of(node: &LayoutNode, index_of: &BTreeMap<PaneId, usize>) -> Node {
    match node {
        LayoutNode::Pane(pane) => Node::Pane(index_of[pane]),
        LayoutNode::Split {
            axis,
            ratio,
            first,
            second,
        } => Node::Split {
            axis: match axis {
                Axis::Horizontal => SplitAxis::Horizontal,
                Axis::Vertical => SplitAxis::Vertical,
            },
            ratio: *ratio,
            first: Box::new(node_of(first, index_of)),
            second: Box::new(node_of(second, index_of)),
        },
    }
}

/// Rebuilds the tree over the panes that came back, lifting the sibling of
/// every pane that did not.
fn build(node: &Node, ids: &[Option<PaneId>]) -> Option<LayoutNode> {
    match node {
        Node::Pane(index) => ids.get(*index).copied().flatten().map(LayoutNode::Pane),
        Node::Split {
            axis,
            ratio,
            first,
            second,
        } => match (build(first, ids), build(second, ids)) {
            (Some(first), Some(second)) => Some(LayoutNode::Split {
                axis: match axis {
                    SplitAxis::Horizontal => Axis::Horizontal,
                    SplitAxis::Vertical => Axis::Vertical,
                },
                ratio: *ratio,
                first: Box::new(first),
                second: Box::new(second),
            }),
            (Some(only), None) | (None, Some(only)) => Some(only),
            (None, None) => None,
        },
    }
}

/// The session with carets and viewports blanked: what must change for a
/// write before shutdown.
fn shape_key(session: &Session) -> String {
    let mut shape = session.clone();
    for pane in &mut shape.panes {
        for tab in &mut pane.tabs {
            tab.row = 0;
            tab.column = 0;
            tab.top_row = 0;
        }
    }
    serde_json::to_string(&shape).unwrap_or_default()
}

fn write_atomically(file: &Path, session: &Session) -> Result<()> {
    let parent = file.parent().context("session file has no directory")?;
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
    let content = serde_json::to_vec_pretty(session).context("serialize the session")?;
    let temporary = file.with_extension("json.tmp");
    fs::write(&temporary, content)
        .with_context(|| format!("could not write {}", temporary.display()))?;
    fs::rename(&temporary, file).with_context(|| format!("could not replace {}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(path: &str) -> Tab {
        Tab {
            path: PathBuf::from(path),
            row: 3,
            column: 1,
            top_row: 2,
        }
    }

    #[test]
    fn missing_panes_collapse_into_their_siblings() {
        let layout = Node::Split {
            axis: SplitAxis::Horizontal,
            ratio: 400,
            first: Box::new(Node::Pane(0)),
            second: Box::new(Node::Split {
                axis: SplitAxis::Vertical,
                ratio: 500,
                first: Box::new(Node::Pane(1)),
                second: Box::new(Node::Pane(2)),
            }),
        };
        let ids = [Some(PaneId(1)), None, Some(PaneId(3))];
        assert_eq!(
            build(&layout, &ids),
            Some(LayoutNode::Split {
                axis: Axis::Horizontal,
                ratio: 400,
                first: Box::new(LayoutNode::Pane(PaneId(1))),
                second: Box::new(LayoutNode::Pane(PaneId(3))),
            })
        );
        assert_eq!(build(&layout, &[None, None, None]), None);
        assert_eq!(build(&Node::Pane(9), &ids), None);
    }

    #[test]
    fn shape_ignores_carets_and_round_trips_through_json() {
        let session = Session {
            version: VERSION,
            layout: Node::Pane(0),
            panes: vec![PaneSession {
                tabs: vec![tab("/repo/a.rs")],
                active: 0,
            }],
            active_pane: 0,
        };
        let mut moved = session.clone();
        moved.panes[0].tabs[0].row = 40;
        assert_eq!(shape_key(&session), shape_key(&moved));
        let mut added = session.clone();
        added.panes[0].tabs.push(tab("/repo/b.rs"));
        assert_ne!(shape_key(&session), shape_key(&added));

        let json = serde_json::to_string(&session).unwrap();
        assert!(json.contains("\"pane\":0"));
        assert_eq!(serde_json::from_str::<Session>(&json).unwrap(), session);
    }
}
