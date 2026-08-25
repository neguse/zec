//! Path-level recording wrapper around Zed's filesystem authority.

use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use async_tar::Archive;
use futures::{AsyncRead, Stream};
use git::repository::GitRepository;
use rope::Rope;
use text::LineEnding;
use zed_fs::{
    CopyOptions, CreateOptions, FileHandle, Fs, JobEventReceiver, Metadata, PathEvent,
    RemoveOptions, RenameOptions, TrashId, TrashRestoreError, Watcher,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FsPathKind {
    Open,
    Stat,
    ReadDir,
    Observe,
    Mutate,
    Repository,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FsPathAccess {
    pub(crate) kind: FsPathKind,
    pub(crate) method: &'static str,
    pub(crate) path: PathBuf,
}

#[derive(Default)]
struct TraceState {
    accesses: Mutex<Vec<FsPathAccess>>,
}

impl TraceState {
    fn record(&self, kind: FsPathKind, method: &'static str, path: &Path) {
        self.accesses
            .lock()
            .expect("filesystem trace mutex poisoned")
            .push(FsPathAccess {
                kind,
                method,
                path: path.to_path_buf(),
            });
    }
}

pub(crate) fn classify_single_file_accesses(
    accesses: &[FsPathAccess],
    allowed: &Path,
) -> Result<Vec<&'static str>> {
    let git_markers = allowed
        .ancestors()
        .map(|ancestor| ancestor.join(".git"))
        .collect::<Vec<_>>();
    let mut operations = BTreeSet::new();
    for access in accesses {
        let operation = match access.kind {
            FsPathKind::Open if access.path == allowed => "open-self",
            FsPathKind::Stat if access.path == allowed => "stat-self",
            FsPathKind::Stat if git_markers.contains(&access.path) => "stat-ancestor-git",
            _ => bail!(
                "filesystem {} ({:?}) escaped the single-file contract at {} (file {})",
                access.method,
                access.kind,
                access.path.display(),
                allowed.display()
            ),
        };
        operations.insert(operation);
    }
    Ok(operations.into_iter().collect())
}

/// zec's filesystem boundary.
///
/// Besides optionally recording path access for the Alpha 1 single-file
/// oracle, this rejects directories that merely happen to be named `.git`
/// before Zed's GitStore starts a repository worker for them.
pub(crate) struct ZecFs {
    inner: Arc<dyn Fs>,
    state: Arc<TraceState>,
    record_accesses: bool,
    isolate_observers: bool,
}

impl ZecFs {
    /// Build the production wrapper. Filesystem behavior, including
    /// `Fs::is_fake`, otherwise remains identical to the wrapped authority.
    pub(crate) fn guarded(inner: Arc<dyn Fs>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            state: Arc::default(),
            record_accesses: false,
            isolate_observers: false,
        })
    }

    /// Build a real-filesystem recorder that suppresses Zed's process-global
    /// filesystem observers. Zed uses `Fs::is_fake` only to skip its global Git
    /// configuration watches in production builds; every filesystem operation
    /// still delegates to `inner`.
    pub(crate) fn isolated_recording(inner: Arc<dyn Fs>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            state: Arc::default(),
            record_accesses: true,
            isolate_observers: true,
        })
    }

    pub(crate) fn clear(&self) {
        self.state
            .accesses
            .lock()
            .expect("filesystem trace mutex poisoned")
            .clear();
    }

    pub(crate) fn accesses(&self) -> Vec<FsPathAccess> {
        self.state
            .accesses
            .lock()
            .expect("filesystem trace mutex poisoned")
            .clone()
    }

    fn record(&self, kind: FsPathKind, method: &'static str, path: &Path) {
        if self.record_accesses {
            self.state.record(kind, method, path);
        }
    }
}

fn resolve_git_metadata_dir(abs_dot_git: &Path) -> Result<PathBuf> {
    if !abs_dot_git.is_file() {
        return Ok(abs_dot_git.to_path_buf());
    }

    let contents = std::fs::read_to_string(abs_dot_git)
        .with_context(|| format!("read Git metadata file {}", abs_dot_git.display()))?;
    let relative_or_absolute = contents
        .strip_prefix("gitdir:")
        .context("Git metadata file does not start with `gitdir:`")?
        .trim();
    ensure!(
        !relative_or_absolute.is_empty(),
        "Git metadata file has an empty gitdir"
    );
    let git_dir = PathBuf::from(relative_or_absolute);
    if git_dir.is_absolute() {
        Ok(git_dir)
    } else {
        Ok(abs_dot_git
            .parent()
            .context("Git metadata file has no parent")?
            .join(git_dir))
    }
}

fn validate_git_repository_metadata(abs_dot_git: &Path) -> Result<()> {
    let git_dir = resolve_git_metadata_dir(abs_dot_git)?;
    let head = git_dir.join("HEAD");
    ensure!(
        head.is_file(),
        "Git metadata at {} has no HEAD file",
        git_dir.display()
    );
    Ok(())
}

struct RecordingWatcher {
    inner: Arc<dyn Watcher>,
    state: Arc<TraceState>,
}

impl Watcher for RecordingWatcher {
    fn add(&self, path: &Path) -> Result<()> {
        self.state.record(FsPathKind::Observe, "watcher.add", path);
        self.inner.add(path)
    }

    fn remove(&self, path: &Path) -> Result<()> {
        self.state
            .record(FsPathKind::Observe, "watcher.remove", path);
        self.inner.remove(path)
    }
}

#[async_trait::async_trait]
impl Fs for ZecFs {
    async fn create_dir(&self, path: &Path) -> Result<()> {
        self.record(FsPathKind::Mutate, "create_dir", path);
        self.inner.create_dir(path).await
    }

    async fn create_symlink(&self, path: &Path, target: PathBuf) -> Result<()> {
        self.record(FsPathKind::Mutate, "create_symlink.path", path);
        self.record(FsPathKind::Mutate, "create_symlink.target", &target);
        self.inner.create_symlink(path, target).await
    }

    async fn create_file(&self, path: &Path, options: CreateOptions) -> Result<()> {
        self.record(FsPathKind::Mutate, "create_file", path);
        self.inner.create_file(path, options).await
    }

    async fn create_file_with(
        &self,
        path: &Path,
        content: Pin<&mut (dyn AsyncRead + Send)>,
    ) -> Result<()> {
        self.record(FsPathKind::Mutate, "create_file_with", path);
        self.inner.create_file_with(path, content).await
    }

    async fn extract_tar_file(
        &self,
        path: &Path,
        content: Archive<Pin<&mut (dyn AsyncRead + Send)>>,
    ) -> Result<()> {
        self.record(FsPathKind::Mutate, "extract_tar_file", path);
        self.inner.extract_tar_file(path, content).await
    }

    async fn copy_file(&self, source: &Path, target: &Path, options: CopyOptions) -> Result<()> {
        self.record(FsPathKind::Open, "copy_file.source", source);
        self.record(FsPathKind::Mutate, "copy_file.target", target);
        self.inner.copy_file(source, target, options).await
    }

    async fn rename(&self, source: &Path, target: &Path, options: RenameOptions) -> Result<()> {
        self.record(FsPathKind::Mutate, "rename.source", source);
        self.record(FsPathKind::Mutate, "rename.target", target);
        self.inner.rename(source, target, options).await
    }

    async fn remove_dir(&self, path: &Path, options: RemoveOptions) -> Result<()> {
        self.record(FsPathKind::Mutate, "remove_dir", path);
        self.inner.remove_dir(path, options).await
    }

    async fn trash(&self, path: &Path, options: RemoveOptions) -> Result<TrashId> {
        self.record(FsPathKind::Mutate, "trash", path);
        self.inner.trash(path, options).await
    }

    async fn remove_file(&self, path: &Path, options: RemoveOptions) -> Result<()> {
        self.record(FsPathKind::Mutate, "remove_file", path);
        self.inner.remove_file(path, options).await
    }

    async fn open_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>> {
        self.record(FsPathKind::Open, "open_handle", path);
        self.inner.open_handle(path).await
    }

    async fn open_sync(&self, path: &Path) -> Result<Box<dyn io::Read + Send + Sync>> {
        self.record(FsPathKind::Open, "open_sync", path);
        self.inner.open_sync(path).await
    }

    async fn load(&self, path: &Path) -> Result<String> {
        self.record(FsPathKind::Open, "load", path);
        self.inner.load(path).await
    }

    async fn load_bytes(&self, path: &Path) -> Result<Vec<u8>> {
        self.record(FsPathKind::Open, "load_bytes", path);
        self.inner.load_bytes(path).await
    }

    async fn atomic_write(&self, path: PathBuf, text: String) -> Result<()> {
        self.record(FsPathKind::Mutate, "atomic_write", &path);
        self.inner.atomic_write(path, text).await
    }

    async fn save(&self, path: &Path, text: &Rope, line_ending: LineEnding) -> Result<()> {
        self.record(FsPathKind::Mutate, "save", path);
        self.inner.save(path, text, line_ending).await
    }

    async fn write(&self, path: &Path, content: &[u8]) -> Result<()> {
        self.record(FsPathKind::Mutate, "write", path);
        self.inner.write(path, content).await
    }

    async fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
        self.record(FsPathKind::Stat, "canonicalize", path);
        self.inner.canonicalize(path).await
    }

    async fn is_file(&self, path: &Path) -> bool {
        self.record(FsPathKind::Stat, "is_file", path);
        self.inner.is_file(path).await
    }

    async fn is_dir(&self, path: &Path) -> bool {
        self.record(FsPathKind::Stat, "is_dir", path);
        self.inner.is_dir(path).await
    }

    async fn metadata(&self, path: &Path) -> Result<Option<Metadata>> {
        self.record(FsPathKind::Stat, "metadata", path);
        self.inner.metadata(path).await
    }

    async fn read_link(&self, path: &Path) -> Result<PathBuf> {
        self.record(FsPathKind::Stat, "read_link", path);
        self.inner.read_link(path).await
    }

    async fn read_dir(
        &self,
        path: &Path,
    ) -> Result<Pin<Box<dyn Send + Stream<Item = Result<PathBuf>>>>> {
        // Every local Zed Project starts an unrelated, process-global cleanup
        // of old js-debug-companion downloads. The Alpha 1 single-file probe
        // uses an isolated filesystem authority so that housekeeping must not
        // race with (or be mistaken for) traversal of the controlled file's
        // parent. Answering this one private directory from an empty in-memory
        // view also guarantees that the probe performs no OS access there.
        if self.isolate_observers && path == paths::debug_adapters_dir().join("js-debug-companion")
        {
            return Ok(Box::pin(futures::stream::empty()));
        }
        self.record(FsPathKind::ReadDir, "read_dir", path);
        self.inner.read_dir(path).await
    }

    async fn watch(
        &self,
        path: &Path,
        latency: Duration,
    ) -> (
        Pin<Box<dyn Send + Stream<Item = Vec<PathEvent>>>>,
        Arc<dyn Watcher>,
    ) {
        self.record(FsPathKind::Observe, "watch", path);
        let (events, watcher) = self.inner.watch(path, latency).await;
        (
            events,
            Arc::new(RecordingWatcher {
                inner: watcher,
                state: self.state.clone(),
            }),
        )
    }

    fn open_repo(
        &self,
        abs_dot_git: &Path,
        system_git_binary_path: Option<&Path>,
    ) -> Result<Arc<dyn GitRepository>> {
        self.record(FsPathKind::Repository, "open_repo.dot_git", abs_dot_git);
        if let Some(path) = system_git_binary_path {
            self.record(FsPathKind::Repository, "open_repo.git_binary", path);
        }
        validate_git_repository_metadata(abs_dot_git).with_context(|| {
            format!(
                "refusing invalid Git repository metadata {}",
                abs_dot_git.display()
            )
        })?;
        self.inner.open_repo(abs_dot_git, system_git_binary_path)
    }

    async fn git_init(
        &self,
        abs_work_directory: &Path,
        fallback_branch_name: String,
    ) -> Result<()> {
        self.record(FsPathKind::Repository, "git_init", abs_work_directory);
        self.inner
            .git_init(abs_work_directory, fallback_branch_name)
            .await
    }

    async fn git_clone(&self, abs_work_directory: &Path, repo_url: &str) -> Result<()> {
        self.record(FsPathKind::Repository, "git_clone", abs_work_directory);
        self.inner.git_clone(abs_work_directory, repo_url).await
    }

    async fn git_config(&self, abs_work_directory: &Path, args: Vec<String>) -> Result<String> {
        self.record(FsPathKind::Repository, "git_config", abs_work_directory);
        self.inner.git_config(abs_work_directory, args).await
    }

    fn is_fake(&self) -> bool {
        self.isolate_observers || self.inner.is_fake()
    }

    async fn is_case_sensitive(&self) -> bool {
        self.inner.is_case_sensitive().await
    }

    fn subscribe_to_jobs(&self) -> JobEventReceiver {
        self.inner.subscribe_to_jobs()
    }

    fn original_path_for_trash_id(&self, trash_id: TrashId) -> Option<PathBuf> {
        self.inner.original_path_for_trash_id(trash_id)
    }

    async fn restore(&self, trash_id: TrashId) -> std::result::Result<PathBuf, TrashRestoreError> {
        self.inner.restore(trash_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_path_as_single_file_access() {
        let target = Path::new("/tmp/zec-alpha-1/outside.txt");
        let parent_access = FsPathAccess {
            kind: FsPathKind::Stat,
            method: "metadata",
            path: PathBuf::from("/tmp/zec-alpha-1"),
        };
        assert!(classify_single_file_accesses(&[parent_access], target).is_err());
    }

    #[test]
    fn classifies_only_exact_ancestor_git_markers() {
        let target = Path::new("/tmp/zec-alpha-1/outside.txt");
        let accesses = [
            FsPathAccess {
                kind: FsPathKind::Open,
                method: "load_bytes",
                path: target.to_path_buf(),
            },
            FsPathAccess {
                kind: FsPathKind::Stat,
                method: "metadata",
                path: PathBuf::from("/tmp/zec-alpha-1/.git"),
            },
        ];
        assert_eq!(
            classify_single_file_accesses(&accesses, target).unwrap(),
            ["open-self", "stat-ancestor-git"]
        );

        let sibling = FsPathAccess {
            kind: FsPathKind::Stat,
            method: "metadata",
            path: PathBuf::from("/tmp/other/.git"),
        };
        assert!(classify_single_file_accesses(&[sibling], target).is_err());

        let observe = FsPathAccess {
            kind: FsPathKind::Observe,
            method: "watch",
            path: target.to_path_buf(),
        };
        assert!(classify_single_file_accesses(&[observe], target).is_err());
    }

    #[test]
    fn rejects_directory_that_only_looks_like_git_metadata() {
        let directory = tempfile::tempdir().expect("temporary Git metadata fixture");
        let dot_git = directory.path().join(".git");
        std::fs::create_dir(&dot_git).expect("create fake .git directory");
        std::fs::write(dot_git.join("alpha1-excluded.txt"), "not a repository")
            .expect("write exclusion sentinel");

        let error = validate_git_repository_metadata(&dot_git)
            .expect_err("a .git directory without HEAD must be rejected");
        assert!(error.to_string().contains("has no HEAD file"));
    }

    #[test]
    fn accepts_normal_and_linked_worktree_git_metadata() {
        let directory = tempfile::tempdir().expect("temporary Git metadata fixture");
        let normal = directory.path().join("normal.git");
        std::fs::create_dir(&normal).expect("create normal Git metadata");
        std::fs::write(normal.join("HEAD"), "ref: refs/heads/main\n").expect("write normal HEAD");
        validate_git_repository_metadata(&normal).expect("accept normal Git metadata");

        let linked = directory.path().join("linked");
        let linked_metadata = directory.path().join("main.git/worktrees/linked");
        std::fs::create_dir_all(&linked_metadata).expect("create linked Git metadata");
        std::fs::create_dir(&linked).expect("create linked worktree");
        std::fs::write(linked_metadata.join("HEAD"), "ref: refs/heads/linked\n")
            .expect("write linked HEAD");
        let dot_git = linked.join(".git");
        std::fs::write(&dot_git, "gitdir: ../main.git/worktrees/linked\n")
            .expect("write linked .git file");
        validate_git_repository_metadata(&dot_git).expect("accept linked Git metadata");
    }
}
