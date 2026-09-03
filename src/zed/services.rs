//! The local Project and the file operations that go through its stores.
//!
//! zec never writes files itself: open, save, Save As, and reload all go
//! through `RealFs -> WorktreeStore -> BufferStore`, which leaves encoding,
//! line endings, disk state, and external change tracking to Zed.

use std::{
    env,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use async_channel::Sender;
use futures::FutureExt as _;
use gpui::{App, AsyncApp, Entity};
use language::{Buffer, LanguageNotFound, LanguageRegistry};
use project::{
    LocalProjectFlags, Project, ProjectEntryId, ProjectPath, Worktree,
    buffer_store::BufferStore,
    lsp_store::{FormatTrigger, LspFormatTarget},
    trusted_worktrees::{PathTrust, TrustedWorktrees, TrustedWorktreesEvent},
    worktree_store::WorktreeStore,
};
use zed_fs::{Fs, Metadata};

use super::runtime::Runtime;

#[derive(Clone)]
pub struct Services {
    pub project: Entity<Project>,
    pub buffer_store: Entity<BufferStore>,
    pub worktree_store: Entity<WorktreeStore>,
    pub fs: Arc<dyn Fs>,
    pub languages: Arc<LanguageRegistry>,
}

impl Services {
    pub fn local(cx: &mut App) -> Self {
        let runtime = cx.global::<Runtime>().clone();
        let mut environment = env::vars().collect::<collections::HashMap<_, _>>();
        #[cfg(windows)]
        normalize_windows_path_environment(&mut environment);
        #[cfg(not(windows))]
        let _ = &mut environment;
        let project = Project::local(
            runtime.client,
            runtime.node_runtime,
            runtime.user_store,
            runtime.languages.clone(),
            runtime.fs.clone(),
            Some(environment),
            LocalProjectFlags {
                init_worktree_trust: false,
                watch_global_configs: true,
            },
            cx,
        );
        let (worktree_store, buffer_store) = project.read_with(cx, |project, _| {
            (project.worktree_store(), project.buffer_store().clone())
        });
        Self {
            project,
            buffer_store,
            worktree_store,
            fs: runtime.fs,
            languages: runtime.languages,
        }
    }

    /// Adds a visible worktree rooted at `directory`.
    pub async fn add_root(&self, directory: &Path, cx: &mut AsyncApp) -> Result<Entity<Worktree>> {
        let (worktree, _) = self
            .project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(directory, true, cx)
            })
            .await
            .with_context(|| format!("could not open {} as a worktree", directory.display()))?;
        Ok(worktree)
    }

    /// The visible worktree, when zec opened a directory.
    pub fn visible_worktree(&self, cx: &AsyncApp) -> Option<Entity<Worktree>> {
        self.worktree_store
            .read_with(cx, |store, cx| store.visible_worktrees(cx).next())
    }

    /// Forwards the project's trust and language server changes as events
    /// for as long as the subscriptions live. Zed marks a worktree
    /// restricted the first time something repository-controlled, such as
    /// a language server, wants to start in it.
    pub fn watch<T>(&self, sender: Sender<T>, cx: &mut AsyncApp) -> Vec<gpui::Subscription>
    where
        T: From<super::Event> + Send + 'static,
    {
        let mut subscriptions = Vec::new();
        let worktree_store = self.worktree_store.clone();
        let trust_sender = sender.clone();
        if let Some(trusted) = cx.update(|cx| TrustedWorktrees::try_get_global(cx)) {
            subscriptions.push(cx.update(|cx| {
                cx.subscribe(&trusted, move |_, event, cx| {
                    let TrustedWorktreesEvent::Restricted(store, paths) = event else {
                        return;
                    };
                    if store.entity_id() != worktree_store.entity_id() {
                        return;
                    }
                    for path in paths {
                        let path = match path {
                            PathTrust::Worktree(id) => worktree_store
                                .read(cx)
                                .worktree_for_id(*id, cx)
                                .map(|worktree| worktree.read(cx).abs_path().to_path_buf()),
                            PathTrust::AbsPath(path) => Some(path.clone()),
                        };
                        if let Some(path) = path {
                            let _ = trust_sender
                                .try_send(super::Event::WorktreeRestricted { path }.into());
                        }
                    }
                })
            }));
        }
        subscriptions.push(cx.update(|cx| {
            cx.subscribe(&self.project, move |_, event, _| {
                let (name, running) = match event {
                    project::Event::LanguageServerAdded(_, name, _) => (name.to_string(), true),
                    project::Event::LanguageServerRemoved(id) => (format!("#{}", id.0), false),
                    _ => return,
                };
                let _ = sender.try_send(super::Event::LanguageServer { name, running }.into());
            })
        }));
        subscriptions
    }

    /// The visible root, when Zed has marked it restricted. Zed decides
    /// that while the worktree is added, before any watcher can hear it.
    pub fn restricted_root(&self, cx: &AsyncApp) -> Option<PathBuf> {
        let worktree = self.visible_worktree(cx)?;
        let restricted = cx.update(|cx| {
            TrustedWorktrees::try_get_global(cx).is_some_and(|trusted| {
                let id = worktree.read(cx).id();
                trusted
                    .read(cx)
                    .restricted_worktrees(&self.worktree_store, cx)
                    .into_iter()
                    .any(|(restricted, _)| restricted == id)
            })
        });
        restricted.then(|| worktree.read_with(cx, |worktree, _| worktree.abs_path().to_path_buf()))
    }

    /// Trusts the visible root, letting Zed start language servers and
    /// other repository-controlled processes in it for this process.
    pub fn trust_root(&self, cx: &mut AsyncApp) -> Result<PathBuf> {
        let worktree = self
            .visible_worktree(cx)
            .context("no directory root to trust")?;
        let (id, path) = worktree.read_with(cx, |worktree, _| {
            (worktree.id(), worktree.abs_path().to_path_buf())
        });
        let trusted = cx
            .update(|cx| TrustedWorktrees::try_get_global(cx))
            .context("worktree trust is not initialized")?;
        let worktree_store = self.worktree_store.clone();
        trusted.update(cx, |trusted, cx| {
            trusted.trust(
                &worktree_store,
                collections::HashSet::from_iter([PathTrust::Worktree(id)]),
                cx,
            );
        });
        Ok(path)
    }

    /// Creates a file or directory at `path` through the project, so the
    /// worktree learns about it at once.
    pub async fn create_entry(
        &self,
        path: &Path,
        is_directory: bool,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let project_path = self.project_path_in_worktree(path, cx)?;
        self.project
            .update(cx, |project, cx| {
                project.create_entry(project_path, is_directory, cx)
            })
            .await
            .with_context(|| format!("could not create {}", path.display()))?;
        Ok(())
    }

    pub async fn rename_entry(
        &self,
        entry: ProjectEntryId,
        path: &Path,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let project_path = self.project_path_in_worktree(path, cx)?;
        self.project
            .update(cx, |project, cx| {
                project.rename_entry(entry, project_path, cx)
            })
            .await
            .with_context(|| format!("could not rename to {}", path.display()))?;
        Ok(())
    }

    pub async fn delete_entry(&self, entry: ProjectEntryId, cx: &mut AsyncApp) -> Result<()> {
        self.project
            .update(cx, |project, cx| project.delete_entry(entry, cx))
            .context("entry is not in a worktree")?
            .await
            .context("could not delete the entry")
    }

    /// The project path of `path` inside an existing worktree.
    fn project_path_in_worktree(&self, path: &Path, cx: &AsyncApp) -> Result<ProjectPath> {
        self.worktree_store
            .read_with(cx, |store, cx| {
                store.project_path_for_absolute_path(path, cx)
            })
            .with_context(|| format!("{} is outside the project", path.display()))
    }

    pub async fn metadata(&self, path: &Path) -> Result<Option<Metadata>> {
        self.fs
            .metadata(path)
            .await
            .with_context(|| format!("could not inspect {}", path.display()))
    }

    /// Opens or creates the buffer for `path`; a missing file becomes a
    /// new buffer that the first save creates.
    pub async fn open_file(&self, path: &Path, cx: &mut AsyncApp) -> Result<Entity<Buffer>> {
        let (project_path, worktree) = self.project_path(path, cx).await?;
        let buffer = self
            .buffer_store
            .update(cx, |store, cx| store.open_buffer(project_path, cx))
            .await
            .with_context(|| format!("could not load {}", path.display()))?;
        self.assign_language(path, &buffer, cx).await?;
        drop(worktree);
        Ok(buffer)
    }

    /// A scratch buffer created through the store so a later Save As
    /// transitions into normal file tracking.
    pub fn create_scratch(&self, cx: &mut App) -> Entity<Buffer> {
        let buffer = self.buffer_store.update(cx, |store, cx| {
            store.create_local_buffer("", None, false, cx)
        });
        buffer
            .read(cx)
            .set_language_registry(self.languages.clone());
        buffer
    }

    /// Saves `buffer` the way Zed's editor does: format-on-save first, which
    /// also applies the trailing-whitespace and final-newline rules, then the
    /// write. As in Zed, a formatter failure or timeout is logged and the
    /// write still happens.
    pub async fn save(&self, buffer: &Entity<Buffer>, cx: &mut AsyncApp) -> Result<()> {
        let path = buffer_state(buffer, cx)
            .path
            .context("buffer has no file path")?;
        self.format_on_save(buffer, cx).await;
        self.buffer_store
            .update(cx, |store, cx| store.save_buffer(buffer.clone(), cx))
            .await
            .with_context(|| format!("could not save {}", path.display()))
    }

    async fn format_on_save(&self, buffer: &Entity<Buffer>, cx: &mut AsyncApp) {
        const FORMAT_TIMEOUT: Duration = Duration::from_secs(5);
        let format = self.project.update(cx, |project, cx| {
            project.format(
                collections::HashSet::from_iter([buffer.clone()]),
                LspFormatTarget::Buffers,
                true,
                FormatTrigger::Save,
                cx,
            )
        });
        let timeout = cx.background_executor().timer(FORMAT_TIMEOUT);
        futures::select_biased! {
            result = format.fuse() => {
                if let Err(error) = result {
                    log::warn!("format on save failed: {error:#}");
                }
            }
            () = timeout.fuse() => log::warn!("timed out waiting for formatting"),
        }
    }

    /// Binds `buffer` to `path` and saves it. Refuses a path another open
    /// buffer already owns.
    pub async fn save_as(
        &self,
        buffer: &Entity<Buffer>,
        path: &Path,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let (project_path, worktree) = self.project_path(path, cx).await?;
        let existing = self
            .buffer_store
            .read_with(cx, |store, _| store.get_by_path(&project_path));
        if existing.is_some_and(|existing| existing != *buffer) {
            bail!("{} is already open in another tab", path.display());
        }
        self.buffer_store
            .update(cx, |store, cx| {
                store.save_buffer_as(buffer.clone(), project_path, cx)
            })
            .await
            .with_context(|| format!("could not save {}", path.display()))?;
        // No LspStore drives language selection here, so the new extension
        // is applied explicitly.
        self.assign_language(path, buffer, cx).await?;
        drop(worktree);
        Ok(())
    }

    pub async fn reload(&self, buffer: &Entity<Buffer>, cx: &mut AsyncApp) -> Result<()> {
        let path = buffer_state(buffer, cx)
            .path
            .context("buffer has no file path")?;
        self.buffer_store
            .update(cx, |store, cx| {
                store.reload_buffers([buffer.clone()].into_iter().collect(), true, cx)
            })
            .await
            .with_context(|| format!("could not reload {}", path.display()))?;
        Ok(())
    }

    /// Resolves `path` inside a worktree, creating an invisible one rooted at
    /// the nearest existing directory. A directory worktree keeps external
    /// renames tracked; the filesystem root is never scanned recursively.
    async fn project_path(
        &self,
        path: &Path,
        cx: &mut AsyncApp,
    ) -> Result<(ProjectPath, Entity<Worktree>)> {
        let mut candidate = path.parent();
        let worktree_root = loop {
            let Some(directory) = candidate.filter(|directory| directory.parent().is_some()) else {
                break path.to_path_buf();
            };
            match self.metadata(directory).await? {
                Some(metadata) if metadata.is_dir => break directory.to_path_buf(),
                _ => candidate = directory.parent(),
            }
        };

        let (worktree, _) = self
            .project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(&worktree_root, false, cx)
            })
            .await
            .with_context(|| format!("could not create a worktree for {}", path.display()))?;
        let project_path = self
            .worktree_store
            .read_with(cx, |store, cx| {
                store.project_path_for_absolute_path(path, cx)
            })
            .with_context(|| format!("worktree does not contain {}", path.display()))?;
        Ok((project_path, worktree))
    }

    async fn assign_language(
        &self,
        path: &Path,
        buffer: &Entity<Buffer>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let language = match self.languages.load_language_for_file_path(path).await {
            Ok(language) => Some(language),
            Err(error) if error.is::<LanguageNotFound>() => None,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("could not select a language for {}", path.display())
                });
            }
        };
        let registry = self.languages.clone();
        buffer.update(cx, move |buffer, cx| {
            buffer.set_language_registry(registry);
            if let Some(language) = language {
                buffer.set_language_async(Some(language), cx);
            }
        });
        Ok(())
    }
}

/// Windows environment-variable names are case-insensitive, but the map Zed
/// receives is not. `std::env::vars` normally yields `Path` on Windows while
/// adapter discovery looks up `PATH`, so canonicalize that one key.
#[cfg(any(windows, test))]
fn normalize_windows_path_environment(environment: &mut collections::HashMap<String, String>) {
    if environment.contains_key("PATH") {
        environment.retain(|key, _| key == "PATH" || !key.eq_ignore_ascii_case("PATH"));
        return;
    }
    let path_key = environment
        .keys()
        .find(|key| key.eq_ignore_ascii_case("PATH"))
        .cloned();
    if let Some(path_key) = path_key
        && let Some(path) = environment.remove(&path_key)
    {
        environment.insert("PATH".to_owned(), path);
    }
}

/// What the buffer's file says right now; never cached.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BufferState {
    pub path: Option<PathBuf>,
    pub dirty: bool,
    pub conflict: bool,
    pub deleted: bool,
}

impl BufferState {
    pub fn needs_discard_confirmation(&self) -> bool {
        self.dirty || self.deleted
    }

    pub fn has_external_change(&self) -> bool {
        self.conflict || self.deleted
    }
}

pub fn buffer_state(buffer: &Entity<Buffer>, cx: &AsyncApp) -> BufferState {
    buffer.read_with(cx, |buffer, cx| {
        let (path, deleted) = match buffer.file() {
            Some(file) => {
                let path = file
                    .as_local()
                    .map(|file| file.abs_path(cx))
                    .unwrap_or_else(|| file.full_path(cx));
                (Some(path), file.disk_state().is_deleted())
            }
            None => (None, false),
        };
        BufferState {
            path,
            dirty: buffer.is_dirty(),
            conflict: buffer.has_conflict(),
            deleted,
        }
    })
}

/// Forwards every change of the worktree's entries as a redraw for as long
/// as the subscription lives.
pub fn watch_worktree<T>(
    worktree: &Entity<Worktree>,
    sender: Sender<T>,
    cx: &mut AsyncApp,
) -> gpui::Subscription
where
    T: From<super::Event> + Send + 'static,
{
    cx.update(|cx| {
        cx.subscribe(worktree, move |_, _event, _| {
            let _ = sender.try_send(super::Event::Redraw.into());
        })
    })
}

/// One symbol of a buffer's outline, in document order.
pub struct OutlineRow {
    pub depth: usize,
    pub text: String,
    /// Where the symbol's name starts.
    pub point: text::Point,
}

/// Zed's outline of `buffer`, empty when its language has no outline
/// query or the buffer has not been parsed yet.
pub fn outline(buffer: &Entity<Buffer>, cx: &AsyncApp) -> Vec<OutlineRow> {
    buffer.read_with(cx, |buffer, _| {
        let snapshot = buffer.snapshot();
        snapshot
            .outline(None)
            .items
            .iter()
            .map(|item| {
                let item = item.to_point(&snapshot);
                OutlineRow {
                    depth: item.depth,
                    text: item.text.to_string(),
                    point: item.selection_range.start,
                }
            })
            .collect()
    })
}

pub fn buffer_id(buffer: &Entity<Buffer>, cx: &AsyncApp) -> u64 {
    buffer.read_with(cx, |buffer, _| buffer.remote_id().to_proto())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_project_environment_canonicalizes_the_path_key() {
        let mut environment = [
            ("Path".to_owned(), r"C:\fixture-bin;C:\Windows".to_owned()),
            ("TEMP".to_owned(), r"C:\Temp".to_owned()),
        ]
        .into_iter()
        .collect::<collections::HashMap<_, _>>();
        normalize_windows_path_environment(&mut environment);
        assert_eq!(
            environment.get("PATH").map(String::as_str),
            Some(r"C:\fixture-bin;C:\Windows")
        );
        assert!(!environment.contains_key("Path"));
        assert_eq!(
            environment.get("TEMP").map(String::as_str),
            Some(r"C:\Temp")
        );
    }

    #[test]
    fn windows_project_environment_prefers_an_existing_canonical_path() {
        let mut environment = [
            ("PATH".to_owned(), "canonical".to_owned()),
            ("Path".to_owned(), "alias".to_owned()),
        ]
        .into_iter()
        .collect::<collections::HashMap<_, _>>();
        normalize_windows_path_environment(&mut environment);
        assert_eq!(environment.len(), 1);
        assert_eq!(
            environment.get("PATH").map(String::as_str),
            Some("canonical")
        );
    }
}
