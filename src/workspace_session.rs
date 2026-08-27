//! Crash-tolerant, content-verified persistence for terminal workspaces.
//!
//! Generations are immutable files. A crash can therefore leave at most an
//! ignored temporary file; restore scans newest-to-oldest, quarantines corrupt
//! generations, and falls back without overwriting user files.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::Write as _,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
};

use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::workspace_model::{ItemId, WorkspaceModel};

pub(crate) const SESSION_SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_SESSION_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_RECOVERY_BLOB_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_SESSION_ITEMS: usize = 512;
pub(crate) const MAX_NAVIGATION_ENTRIES: usize = 1_024;
const MAX_GENERATION_CANDIDATES: usize = 256;
const HASH_HEX_LEN: usize = 64;
static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "encoding", content = "units", rename_all = "snake_case")]
pub(crate) enum SessionPath {
    UnixBytes(Vec<u8>),
    WindowsWide(Vec<u16>),
}

impl SessionPath {
    pub(crate) fn from_path(path: &Path) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            Self::UnixBytes(path.as_os_str().as_bytes().to_vec())
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt as _;
            Self::WindowsWide(path.as_os_str().encode_wide().collect())
        }
        #[cfg(not(any(unix, windows)))]
        {
            Self::UnixBytes(path.to_string_lossy().as_bytes().to_vec())
        }
    }

    pub(crate) fn to_path_buf(&self) -> Result<PathBuf> {
        match self {
            Self::UnixBytes(bytes) => {
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStringExt as _;
                    Ok(PathBuf::from(OsString::from_vec(bytes.clone())))
                }
                #[cfg(not(unix))]
                {
                    let _ = bytes;
                    bail!("Unix session path cannot be restored on this platform")
                }
            }
            Self::WindowsWide(units) => {
                #[cfg(windows)]
                {
                    use std::os::windows::ffi::OsStringExt as _;
                    Ok(PathBuf::from(OsString::from_wide(units)))
                }
                #[cfg(not(windows))]
                {
                    let _ = units;
                    bail!("Windows session path cannot be restored on this platform")
                }
            }
        }
    }

    fn encoded_len(&self) -> usize {
        match self {
            Self::UnixBytes(bytes) => bytes.len(),
            Self::WindowsWide(units) => units.len().saturating_mul(2),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionItemKind {
    Editor,
    Image,
    MultiBuffer,
    Recovered,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SessionPoint {
    pub(crate) row: u32,
    pub(crate) column: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SessionSelection {
    pub(crate) anchor: SessionPoint,
    pub(crate) head: SessionPoint,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SessionViewport {
    pub(crate) row: usize,
    pub(crate) column_cells: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SessionExcerpt {
    pub(crate) context_start: SessionPoint,
    pub(crate) context_end: SessionPoint,
    pub(crate) primary_start: SessionPoint,
    pub(crate) primary_end: SessionPoint,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionSource {
    pub(crate) path: Option<SessionPath>,
    pub(crate) recovery_sha256: Option<String>,
    pub(crate) excerpts: Vec<SessionExcerpt>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionItem {
    pub(crate) id: ItemId,
    pub(crate) kind: SessionItemKind,
    pub(crate) path: Option<SessionPath>,
    pub(crate) source_paths: Vec<SessionPath>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) sources: Vec<SessionSource>,
    pub(crate) untitled_label: Option<String>,
    pub(crate) dirty_recovery_sha256: Option<String>,
    pub(crate) selections: Vec<SessionSelection>,
    pub(crate) viewport: SessionViewport,
    pub(crate) folded_ranges: Vec<(SessionPoint, SessionPoint)>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) soft_wrap: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NavigationEntry {
    pub(crate) item: ItemId,
    pub(crate) path: Option<SessionPath>,
    pub(crate) selection: SessionSelection,
    pub(crate) viewport: SessionViewport,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkspaceSession {
    pub(crate) repository: Option<SessionPath>,
    pub(crate) workspace: WorkspaceModel,
    pub(crate) items: BTreeMap<ItemId, SessionItem>,
    pub(crate) navigation_back: Vec<NavigationEntry>,
    pub(crate) navigation_forward: Vec<NavigationEntry>,
}

impl WorkspaceSession {
    pub(crate) fn validate(&self) -> Result<()> {
        self.workspace
            .validate()
            .context("invalid persisted workspace layout")?;
        ensure!(
            self.workspace.overlays.is_empty(),
            "transient overlays must not be persisted"
        );
        ensure!(
            self.items.len() <= MAX_SESSION_ITEMS,
            "session has too many items"
        );
        ensure!(
            self.workspace.item_ids() == self.items.keys().copied().collect(),
            "session item map differs from workspace item set"
        );
        ensure!(
            self.navigation_back.len() <= MAX_NAVIGATION_ENTRIES
                && self.navigation_forward.len() <= MAX_NAVIGATION_ENTRIES,
            "session navigation history is too large"
        );

        for (id, item) in &self.items {
            ensure!(*id == item.id, "session item key differs from embedded ID");
            ensure!(
                item.untitled_label.as_ref().is_none_or(|label| {
                    !label.is_empty() && label.len() <= 1_024 && !label.contains(['\n', '\r'])
                }),
                "session item has an invalid untitled label"
            );
            ensure!(
                item.path
                    .as_ref()
                    .is_none_or(|path| path.encoded_len() <= 32_768)
                    && item
                        .source_paths
                        .iter()
                        .all(|path| path.encoded_len() <= 32_768),
                "session item path is too large"
            );
            if let Some(digest) = &item.dirty_recovery_sha256 {
                ensure!(is_sha256(digest), "invalid recovery blob SHA-256");
            }
            if item.kind == SessionItemKind::Image {
                ensure!(
                    item.path.is_some() && item.dirty_recovery_sha256.is_none(),
                    "image session item requires a path and cannot contain text recovery"
                );
            }
            ensure!(
                item.sources.len() <= MAX_SESSION_ITEMS,
                "session item has too many MultiBuffer sources"
            );
            let mut excerpt_count = 0usize;
            for source in &item.sources {
                ensure!(
                    source
                        .path
                        .as_ref()
                        .is_none_or(|path| path.encoded_len() <= 32_768),
                    "session MultiBuffer source path is too large"
                );
                if let Some(digest) = &source.recovery_sha256 {
                    ensure!(is_sha256(digest), "invalid source recovery blob SHA-256");
                }
                excerpt_count = excerpt_count
                    .checked_add(source.excerpts.len())
                    .context("session MultiBuffer excerpt count overflow")?;
                for excerpt in &source.excerpts {
                    ensure!(
                        point_le(excerpt.context_start, excerpt.context_end)
                            && point_le(excerpt.primary_start, excerpt.primary_end),
                        "session MultiBuffer excerpt range is reversed"
                    );
                    ensure!(
                        point_le(excerpt.context_start, excerpt.primary_start)
                            && point_le(excerpt.primary_end, excerpt.context_end),
                        "session MultiBuffer primary range escapes its context"
                    );
                }
            }
            ensure!(
                excerpt_count <= 100_000,
                "session item has too many MultiBuffer excerpts"
            );
            ensure!(
                item.kind == SessionItemKind::MultiBuffer || item.sources.is_empty(),
                "non-MultiBuffer session item contains excerpt sources"
            );
            ensure!(
                item.selections.len() <= 1_024 && item.folded_ranges.len() <= 100_000,
                "session editor presentation state is too large"
            );
            for (start, end) in &item.folded_ranges {
                ensure!(point_le(*start, *end), "session fold range is reversed");
            }
        }

        for navigation in self.navigation_back.iter().chain(&self.navigation_forward) {
            ensure!(
                self.items.contains_key(&navigation.item) || navigation.path.is_some(),
                "navigation entry has neither a live item nor a path"
            );
        }
        Ok(())
    }
}

fn point_le(left: SessionPoint, right: SessionPoint) -> bool {
    (left.row, left.column) <= (right.row, right.column)
}

const fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct SessionEnvelope {
    schema_version: u32,
    generation: u64,
    payload_sha256: String,
    payload: WorkspaceSession,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RejectedGeneration {
    pub(crate) path: PathBuf,
    pub(crate) reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LoadedSession {
    pub(crate) generation: u64,
    pub(crate) path: PathBuf,
    pub(crate) session: WorkspaceSession,
    pub(crate) rejected_newer: Vec<RejectedGeneration>,
}

#[derive(Clone, Debug)]
pub(crate) struct SessionStore {
    directory: PathBuf,
}

impl SessionStore {
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    pub(crate) fn write_generation(
        &self,
        key: &str,
        generation: u64,
        session: &WorkspaceSession,
    ) -> Result<PathBuf> {
        validate_key(key)?;
        ensure!(generation > 0, "session generation must be positive");
        session.validate()?;
        self.prepare_directory()?;

        let payload_bytes = serde_json::to_vec(session).context("serialize session payload")?;
        let payload_sha256 = sha256_hex(&payload_bytes);
        let envelope = SessionEnvelope {
            schema_version: SESSION_SCHEMA_VERSION,
            generation,
            payload_sha256: payload_sha256.clone(),
            payload: session.clone(),
        };
        let mut bytes =
            serde_json::to_vec_pretty(&envelope).context("serialize session envelope")?;
        bytes.push(b'\n');
        ensure!(
            bytes.len() <= MAX_SESSION_BYTES,
            "session exceeds byte limit"
        );

        let final_path = self
            .directory
            .join(format!("{key}-{generation:020}-{payload_sha256}.json"));
        ensure!(
            fs::symlink_metadata(&final_path).is_err(),
            "session generation already exists"
        );
        let temporary = self.temporary_path(key, "session");
        let result = (|| -> Result<()> {
            let mut file = create_private_new_file(&temporary)?;
            file.write_all(&bytes).context("write temporary session")?;
            file.sync_all().context("sync temporary session")?;
            fs::rename(&temporary, &final_path).with_context(|| {
                format!(
                    "commit session {} to {}",
                    temporary.display(),
                    final_path.display()
                )
            })?;
            sync_directory(&self.directory)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        Ok(final_path)
    }

    pub(crate) fn load_latest(&self, key: &str) -> Result<Option<LoadedSession>> {
        validate_key(key)?;
        let metadata = match fs::symlink_metadata(&self.directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("stat session directory"),
        };
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "unsafe session directory"
        );

        let mut candidates = Vec::new();
        let prefix = format!("{key}-");
        for entry in fs::read_dir(&self.directory).context("read session directory")? {
            let entry = entry.context("read session directory entry")?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some((generation, filename_hash)) = parse_generation_name(name, &prefix) else {
                continue;
            };
            candidates.push((generation, filename_hash.to_owned(), entry.path()));
            ensure!(
                candidates.len() <= MAX_GENERATION_CANDIDATES,
                "too many session generations"
            );
        }
        candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.2.cmp(&right.2)));

        let mut rejected_newer = Vec::new();
        for (generation, filename_hash, path) in candidates {
            match load_generation(&path, generation, &filename_hash) {
                Ok(session) => {
                    return Ok(Some(LoadedSession {
                        generation,
                        path,
                        session,
                        rejected_newer,
                    }));
                }
                Err(error) => {
                    let mut reason = bounded_error(&error);
                    let rejected_path = match self.quarantine_generation(&path) {
                        Ok(path) => path,
                        Err(quarantine_error) => {
                            reason.push_str("; quarantine failed: ");
                            reason.push_str(&bounded_error(&quarantine_error));
                            path
                        }
                    };
                    rejected_newer.push(RejectedGeneration {
                        path: rejected_path,
                        reason,
                    });
                }
            }
        }
        if rejected_newer.is_empty() {
            Ok(None)
        } else {
            bail!(
                "no valid session generation; newest rejection: {}",
                rejected_newer[0].reason
            )
        }
    }

    pub(crate) fn next_generation(&self, key: &str) -> Result<u64> {
        validate_key(key)?;
        let metadata = match fs::symlink_metadata(&self.directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(1),
            Err(error) => return Err(error).context("stat session directory"),
        };
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "unsafe session directory"
        );
        let prefix = format!("{key}-");
        let highest = fs::read_dir(&self.directory)
            .context("read session directory")?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name();
                parse_generation_name(name.to_str()?, &prefix).map(|(generation, _)| generation)
            })
            .max()
            .unwrap_or_default();
        highest
            .checked_add(1)
            .context("session generation space exhausted")
    }

    pub(crate) fn store_recovery_blob(&self, bytes: &[u8]) -> Result<String> {
        ensure!(
            bytes.len() <= MAX_RECOVERY_BLOB_BYTES,
            "recovery blob exceeds byte limit"
        );
        self.prepare_directory()?;
        let digest = sha256_hex(bytes);
        let final_path = self.directory.join(format!("recovery-{digest}.blob"));
        if fs::symlink_metadata(&final_path).is_ok() {
            ensure!(
                self.load_recovery_blob(&digest)? == bytes,
                "existing recovery blob does not match its digest"
            );
            return Ok(digest);
        }

        let temporary = self.temporary_path("recovery", "blob");
        let result = (|| -> Result<()> {
            let mut file = create_private_new_file(&temporary)?;
            file.write_all(bytes).context("write recovery blob")?;
            file.sync_all().context("sync recovery blob")?;
            match fs::rename(&temporary, &final_path) {
                Ok(()) => {}
                Err(error) if fs::symlink_metadata(&final_path).is_ok() => {
                    let _ = error;
                    fs::remove_file(&temporary).context("remove raced recovery temporary")?;
                }
                Err(error) => return Err(error).context("commit recovery blob"),
            }
            sync_directory(&self.directory)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        ensure!(
            self.load_recovery_blob(&digest)? == bytes,
            "committed recovery blob failed readback"
        );
        Ok(digest)
    }

    pub(crate) fn load_recovery_blob(&self, digest: &str) -> Result<Vec<u8>> {
        ensure!(is_sha256(digest), "invalid recovery blob SHA-256");
        let path = self.directory.join(format!("recovery-{digest}.blob"));
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("stat recovery blob {}", path.display()))?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "unsafe recovery blob"
        );
        ensure!(
            metadata.len() <= MAX_RECOVERY_BLOB_BYTES as u64,
            "recovery blob exceeds byte limit"
        );
        let bytes = fs::read(&path).context("read recovery blob")?;
        ensure!(
            sha256_hex(&bytes) == digest,
            "recovery blob digest mismatch"
        );
        Ok(bytes)
    }

    pub(crate) fn prune_generations(&self, key: &str, retain: usize) -> Result<Vec<PathBuf>> {
        validate_key(key)?;
        ensure!(retain > 0, "must retain at least one session generation");
        let Some(latest) = self.load_latest(key)? else {
            return Ok(Vec::new());
        };
        let prefix = format!("{key}-");
        let mut candidates = fs::read_dir(&self.directory)
            .context("read session directory for pruning")?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_str()?;
                let (generation, _) = parse_generation_name(name, &prefix)?;
                Some((generation, entry.path()))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| right.0.cmp(&left.0));
        let retained_generations = candidates
            .iter()
            .filter(|(generation, _)| *generation <= latest.generation)
            .take(retain)
            .map(|(generation, _)| *generation)
            .collect::<BTreeSet<_>>();
        let mut removed = Vec::new();
        for (generation, path) in candidates {
            if generation <= latest.generation && !retained_generations.contains(&generation) {
                let metadata = fs::symlink_metadata(&path)
                    .with_context(|| format!("stat session generation {}", path.display()))?;
                ensure!(
                    metadata.is_file() && !metadata.file_type().is_symlink(),
                    "refusing to prune unsafe session generation"
                );
                fs::remove_file(&path)
                    .with_context(|| format!("prune session generation {}", path.display()))?;
                removed.push(path);
            }
        }
        sync_directory(&self.directory)?;
        Ok(removed)
    }

    /// Deletes content-addressed recovery blobs that are not referenced by any
    /// valid session generation in this store. Discovery completes before the
    /// first deletion, so an unreadable/corrupt generation makes the operation
    /// fail closed and preserves every blob.
    pub(crate) fn prune_recovery_blobs(&self) -> Result<Vec<PathBuf>> {
        let metadata = match fs::symlink_metadata(&self.directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(error) => return Err(error).context("stat session directory for blob pruning"),
        };
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "unsafe session directory"
        );

        let entries = fs::read_dir(&self.directory)
            .context("read session directory for blob pruning")?
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("read session directory entry for blob pruning")?;
        let mut referenced = BTreeSet::new();
        let mut blobs = Vec::new();
        for entry in &entries {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if let Some((generation, filename_hash)) = parse_any_generation_name(name) {
                let session = load_generation(&entry.path(), generation, filename_hash)
                    .with_context(|| {
                        format!(
                            "refusing recovery blob pruning because session generation {} is invalid",
                            entry.path().display()
                        )
                    })?;
                referenced.extend(session_recovery_digests(&session));
            } else if let Some(digest) = name
                .strip_prefix("recovery-")
                .and_then(|name| name.strip_suffix(".blob"))
                .filter(|digest| is_sha256(digest))
            {
                blobs.push((digest.to_owned(), entry.path()));
            }
        }

        let mut removed = Vec::new();
        for (digest, path) in blobs {
            if referenced.contains(&digest) {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)
                .with_context(|| format!("stat orphan recovery blob {}", path.display()))?;
            ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "refusing to prune unsafe recovery blob"
            );
            fs::remove_file(&path)
                .with_context(|| format!("prune orphan recovery blob {}", path.display()))?;
            removed.push(path);
        }
        if !removed.is_empty() {
            sync_directory(&self.directory)?;
        }
        Ok(removed)
    }

    fn prepare_directory(&self) -> Result<()> {
        fs::create_dir_all(&self.directory)
            .with_context(|| format!("create session directory {}", self.directory.display()))?;
        let metadata = fs::symlink_metadata(&self.directory)
            .with_context(|| format!("stat session directory {}", self.directory.display()))?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "session directory is not a real directory"
        );
        Ok(())
    }

    fn temporary_path(&self, key: &str, suffix: &str) -> PathBuf {
        let nonce = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        self.directory.join(format!(
            ".{key}-{}-{nonce}.{suffix}.tmp",
            std::process::id()
        ))
    }

    fn quarantine_generation(&self, path: &Path) -> Result<PathBuf> {
        ensure!(
            path.parent() == Some(self.directory.as_path()),
            "session quarantine target is outside the store"
        );
        let nonce = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let original = path
            .file_name()
            .context("session quarantine target has no file name")?;
        let name_hash = sha256_hex(original.as_encoded_bytes());
        let destination = self.directory.join(format!(
            ".rejected-{}-{nonce}-{name_hash}.session",
            std::process::id()
        ));
        fs::rename(path, &destination).with_context(|| {
            format!(
                "quarantine rejected session {} as {}",
                path.display(),
                destination.display()
            )
        })?;
        sync_directory(&self.directory)?;
        Ok(destination)
    }
}

#[derive(Debug)]
pub(crate) struct SessionCommitResult {
    pub(crate) generation: u64,
    pub(crate) result: std::result::Result<PathBuf, String>,
}

struct SessionCommit {
    generation: u64,
    session: WorkspaceSession,
    recovery_blobs: BTreeMap<String, Vec<u8>>,
}

enum SessionWriterCommand {
    Commit(SessionCommit),
    Shutdown,
}

/// Serializes durable session commits away from the terminal input thread.
/// Only one background commit is admitted at a time during normal operation;
/// clean shutdown may enqueue one final snapshot behind it and joins the
/// worker before terminal teardown completes.
pub(crate) struct SessionWriter {
    sender: mpsc::Sender<SessionWriterCommand>,
    results: mpsc::Receiver<SessionCommitResult>,
    worker: Option<JoinHandle<()>>,
    next_generation: u64,
    in_flight: usize,
}

#[cfg(unix)]
struct SessionWriterLease {
    _lock: nix::fcntl::Flock<File>,
}

#[cfg(not(unix))]
struct SessionWriterLease {
    _file: File,
    path: PathBuf,
}

#[cfg(not(unix))]
impl Drop for SessionWriterLease {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
struct SessionCommitLease {
    _lock: nix::fcntl::Flock<File>,
}

#[cfg(not(unix))]
struct SessionCommitLease {
    _file: File,
    path: PathBuf,
}

#[cfg(not(unix))]
impl Drop for SessionCommitLease {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl SessionWriter {
    pub(crate) fn start(store: SessionStore, key: String) -> Result<Self> {
        validate_key(&key)?;
        let lease = acquire_session_writer_lease(&store, &key)?;
        let next_generation = store.next_generation(&key)?;
        let (sender, commands) = mpsc::channel();
        let (result_sender, results) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("zec-workspace-session-writer".to_owned())
            .spawn(move || {
                let _lease = lease;
                while let Ok(command) = commands.recv() {
                    match command {
                        SessionWriterCommand::Commit(commit) => {
                            let result = commit_session(&store, &key, &commit)
                                .map_err(|error| format!("{error:#}"));
                            let _ = result_sender.send(SessionCommitResult {
                                generation: commit.generation,
                                result,
                            });
                        }
                        SessionWriterCommand::Shutdown => break,
                    }
                }
            })
            .context("spawn workspace session writer")?;
        Ok(Self {
            sender,
            results,
            worker: Some(worker),
            next_generation,
            in_flight: 0,
        })
    }

    pub(crate) fn try_commit(
        &mut self,
        session: WorkspaceSession,
        recovery_blobs: BTreeMap<String, Vec<u8>>,
    ) -> Result<bool> {
        if self.in_flight != 0 {
            return Ok(false);
        }
        self.enqueue(session, recovery_blobs)?;
        Ok(true)
    }

    pub(crate) fn poll(&mut self) -> Vec<SessionCommitResult> {
        let mut completed = Vec::new();
        while let Ok(result) = self.results.try_recv() {
            self.in_flight = self.in_flight.saturating_sub(1);
            completed.push(result);
        }
        completed
    }

    pub(crate) fn finish(
        mut self,
        final_session: Option<(WorkspaceSession, BTreeMap<String, Vec<u8>>)>,
    ) -> Result<Vec<SessionCommitResult>> {
        if let Some((session, recovery_blobs)) = final_session {
            self.enqueue(session, recovery_blobs)?;
        }
        self.sender
            .send(SessionWriterCommand::Shutdown)
            .context("stop workspace session writer")?;
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("workspace session writer panicked"))?;
        }
        Ok(self.results.try_iter().collect())
    }

    fn enqueue(
        &mut self,
        session: WorkspaceSession,
        recovery_blobs: BTreeMap<String, Vec<u8>>,
    ) -> Result<()> {
        let generation = self.next_generation;
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .context("session generation space exhausted")?;
        self.sender
            .send(SessionWriterCommand::Commit(SessionCommit {
                generation,
                session,
                recovery_blobs,
            }))
            .context("queue workspace session commit")?;
        self.in_flight = self.in_flight.saturating_add(1);
        Ok(())
    }
}

#[cfg(unix)]
fn acquire_session_writer_lease(store: &SessionStore, key: &str) -> Result<SessionWriterLease> {
    use std::os::unix::fs::OpenOptionsExt as _;

    store.prepare_directory()?;
    let path = store.directory.join(format!("{key}.lock"));
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("open workspace session lease {}", path.display()))?;
    ensure!(
        file.metadata()
            .context("inspect workspace session lease")?
            .is_file(),
        "workspace session lease is not a regular file"
    );
    let lock = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock)
        .map_err(|(_, error)| error)
        .with_context(|| {
            format!(
                "another zec process owns workspace session lease {}",
                path.display()
            )
        })?;
    Ok(SessionWriterLease { _lock: lock })
}

#[cfg(unix)]
fn acquire_session_commit_lease(store: &SessionStore) -> Result<SessionCommitLease> {
    use std::os::unix::fs::OpenOptionsExt as _;

    store.prepare_directory()?;
    let path = store.directory.join(".commit.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("open workspace session commit lease {}", path.display()))?;
    ensure!(
        file.metadata()
            .context("inspect workspace session commit lease")?
            .is_file(),
        "workspace session commit lease is not a regular file"
    );
    let lock = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive)
        .map_err(|(_, error)| error)
        .context("acquire workspace session commit lease")?;
    Ok(SessionCommitLease { _lock: lock })
}

#[cfg(not(unix))]
fn acquire_session_commit_lease(store: &SessionStore) -> Result<SessionCommitLease> {
    store.prepare_directory()?;
    let path = store.directory.join(".commit.lock");
    let file = create_private_new_file(&path).context("acquire workspace session commit lease")?;
    Ok(SessionCommitLease { _file: file, path })
}

#[cfg(not(unix))]
fn acquire_session_writer_lease(store: &SessionStore, key: &str) -> Result<SessionWriterLease> {
    store.prepare_directory()?;
    let path = store.directory.join(format!("{key}.lock"));
    let file = create_private_new_file(&path).with_context(|| {
        format!(
            "another zec process owns workspace session lease {}",
            path.display()
        )
    })?;
    Ok(SessionWriterLease { _file: file, path })
}

fn commit_session(store: &SessionStore, key: &str, commit: &SessionCommit) -> Result<PathBuf> {
    // This store-wide lease closes the race where a commit for another
    // repository has written a new blob but not yet published the generation
    // that references it while this commit performs garbage collection.
    let _commit_lease = acquire_session_commit_lease(store)?;
    for (expected_digest, bytes) in &commit.recovery_blobs {
        ensure!(
            sha256_hex(bytes) == *expected_digest,
            "recovery blob differs from captured digest"
        );
        ensure!(
            store.store_recovery_blob(bytes)? == *expected_digest,
            "recovery blob store returned a different digest"
        );
    }
    let path = store.write_generation(key, commit.generation, &commit.session)?;
    store.prune_generations(key, 5)?;
    store.prune_recovery_blobs()?;
    Ok(path)
}

fn load_generation(
    path: &Path,
    expected_generation: u64,
    filename_hash: &str,
) -> Result<WorkspaceSession> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("stat session generation {}", path.display()))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "unsafe session generation file"
    );
    ensure!(
        metadata.len() <= MAX_SESSION_BYTES as u64,
        "session generation exceeds byte limit"
    );
    let bytes = fs::read(path).context("read session generation")?;
    let envelope: SessionEnvelope =
        serde_json::from_slice(&bytes).context("parse session generation")?;
    ensure!(
        envelope.schema_version == SESSION_SCHEMA_VERSION,
        "unsupported session schema version"
    );
    ensure!(
        envelope.generation == expected_generation,
        "session generation differs from filename"
    );
    ensure!(
        envelope.payload_sha256 == filename_hash && is_sha256(&envelope.payload_sha256),
        "session payload digest differs from filename"
    );
    let payload_bytes =
        serde_json::to_vec(&envelope.payload).context("reserialize session payload")?;
    ensure!(
        sha256_hex(&payload_bytes) == envelope.payload_sha256,
        "session payload digest mismatch"
    );
    envelope.payload.validate()?;
    Ok(envelope.payload)
}

fn parse_generation_name<'a>(name: &'a str, prefix: &str) -> Option<(u64, &'a str)> {
    let body = name.strip_prefix(prefix)?.strip_suffix(".json")?;
    let (generation, digest) = body.split_once('-')?;
    if generation.len() != 20 || !generation.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    if !is_sha256(digest) {
        return None;
    }
    Some((generation.parse().ok()?, digest))
}

fn parse_any_generation_name(name: &str) -> Option<(u64, &str)> {
    let body = name.strip_suffix(".json")?;
    let (key_and_generation, digest) = body.rsplit_once('-')?;
    let (key, generation) = key_and_generation.rsplit_once('-')?;
    if validate_key(key).is_err()
        || generation.len() != 20
        || !generation.bytes().all(|byte| byte.is_ascii_digit())
        || !is_sha256(digest)
    {
        return None;
    }
    Some((generation.parse().ok()?, digest))
}

fn session_recovery_digests(session: &WorkspaceSession) -> BTreeSet<String> {
    session
        .items
        .values()
        .flat_map(|item| {
            item.dirty_recovery_sha256
                .iter()
                .chain(
                    item.sources
                        .iter()
                        .filter_map(|source| source.recovery_sha256.as_ref()),
                )
                .cloned()
        })
        .collect()
}

fn validate_key(key: &str) -> Result<()> {
    ensure!(
        !key.is_empty()
            && key.len() <= 128
            && key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "invalid session key"
    );
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == HASH_HEX_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn bounded_error(error: &anyhow::Error) -> String {
    let message = format!("{error:#}");
    let mut end = message.len().min(2_048);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_owned()
}

fn create_private_new_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("create private temporary file {}", path.display()))
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .with_context(|| format!("open directory {} for sync", path.display()))?
        .sync_all()
        .with_context(|| format!("sync directory {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_model::{OpenDisposition, SplitAxis};

    fn session() -> WorkspaceSession {
        let mut workspace = WorkspaceModel::new(ItemId(1));
        workspace
            .open_item(ItemId(2), OpenDisposition::Pinned)
            .unwrap();
        workspace
            .split_active(SplitAxis::Horizontal, ItemId(3), true)
            .unwrap();
        let item = |id, path: &str| SessionItem {
            id: ItemId(id),
            kind: SessionItemKind::Editor,
            path: Some(SessionPath::from_path(Path::new(path))),
            source_paths: Vec::new(),
            sources: Vec::new(),
            untitled_label: None,
            dirty_recovery_sha256: None,
            selections: vec![SessionSelection {
                anchor: SessionPoint { row: 1, column: 2 },
                head: SessionPoint { row: 3, column: 4 },
            }],
            viewport: SessionViewport {
                row: 1,
                column_cells: 0,
            },
            folded_ranges: Vec::new(),
            soft_wrap: false,
        };
        WorkspaceSession {
            repository: Some(SessionPath::from_path(Path::new("/repo"))),
            workspace,
            items: BTreeMap::from([
                (ItemId(1), item(1, "/repo/a.rs")),
                (ItemId(2), item(2, "/repo/b.rs")),
                (ItemId(3), item(3, "/repo/c.rs")),
            ]),
            navigation_back: Vec::new(),
            navigation_forward: Vec::new(),
        }
    }

    #[test]
    fn round_trips_non_utf8_paths_without_loss_on_unix() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt as _;
            let path = PathBuf::from(OsString::from_vec(vec![b'/', b'x', 0xff]));
            let encoded = SessionPath::from_path(&path);
            assert_eq!(encoded.to_path_buf().unwrap(), path);
            assert_eq!(
                serde_json::from_str::<SessionPath>(&serde_json::to_string(&encoded).unwrap())
                    .unwrap(),
                encoded
            );
        }
    }

    #[test]
    fn immutable_generations_round_trip_and_newest_wins() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let first = session();
        store.write_generation("repo", 1, &first).unwrap();
        let mut second = first.clone();
        second.workspace.focus_item(ItemId(2)).unwrap();
        store.write_generation("repo", 2, &second).unwrap();

        let loaded = store.load_latest("repo").unwrap().unwrap();
        assert_eq!(loaded.generation, 2);
        assert_eq!(loaded.session, second);
        assert!(loaded.rejected_newer.is_empty());
    }

    #[test]
    fn corrupt_newest_generation_is_quarantined_before_falling_back() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let expected = session();
        store.write_generation("repo", 1, &expected).unwrap();
        let corrupt =
            store
                .directory
                .join(format!("repo-{:020}-{}.json", 2, "0".repeat(HASH_HEX_LEN)));
        fs::write(&corrupt, b"{truncated").unwrap();

        let loaded = store.load_latest("repo").unwrap().unwrap();
        assert_eq!(loaded.generation, 1);
        assert_eq!(loaded.session, expected);
        assert_eq!(loaded.rejected_newer.len(), 1);
        assert!(!corrupt.exists());
        assert!(loaded.rejected_newer[0].path.exists());
        assert!(
            loaded.rejected_newer[0]
                .path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".rejected-")
        );
    }

    #[test]
    fn temporary_and_unrelated_files_are_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        store.prepare_directory().unwrap();
        fs::write(store.directory.join(".repo-crash.session.tmp"), b"partial").unwrap();
        fs::write(store.directory.join("another-file"), b"unrelated").unwrap();
        assert!(store.load_latest("repo").unwrap().is_none());
    }

    #[test]
    fn payload_hash_and_filename_generation_are_authoritative() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let path = store.write_generation("repo", 1, &session()).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["generation"] = serde_json::json!(2);
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(store.load_latest("repo").is_err());
        assert!(!path.exists());
        assert!(store.load_latest("repo").unwrap().is_none());
    }

    #[test]
    fn recovery_blobs_are_content_addressed_bounded_and_verified() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let content = b"unsaved\0bytes\n";
        let digest = store.store_recovery_blob(content).unwrap();
        assert_eq!(store.store_recovery_blob(content).unwrap(), digest);
        assert_eq!(store.load_recovery_blob(&digest).unwrap(), content);

        let blob = store.directory.join(format!("recovery-{digest}.blob"));
        fs::write(&blob, b"tampered").unwrap();
        assert!(store.load_recovery_blob(&digest).is_err());
        assert!(
            store
                .store_recovery_blob(&vec![0; MAX_RECOVERY_BLOB_BYTES + 1])
                .is_err()
        );
    }

    #[test]
    fn recovery_blob_gc_preserves_every_referenced_workspace_generation() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let first_bytes = b"first dirty buffer";
        let second_bytes = b"second repository dirty buffer";
        let orphan_bytes = b"orphan after a successful save";
        let first_digest = store.store_recovery_blob(first_bytes).unwrap();
        let second_digest = store.store_recovery_blob(second_bytes).unwrap();
        let orphan_digest = store.store_recovery_blob(orphan_bytes).unwrap();

        let mut first = session();
        first
            .items
            .get_mut(&ItemId(1))
            .unwrap()
            .dirty_recovery_sha256 = Some(first_digest.clone());
        store.write_generation("repo-a", 1, &first).unwrap();

        let mut second = session();
        second
            .items
            .get_mut(&ItemId(2))
            .unwrap()
            .dirty_recovery_sha256 = Some(second_digest.clone());
        store.write_generation("repo-b", 1, &second).unwrap();

        let removed = store.prune_recovery_blobs().unwrap();
        assert_eq!(
            removed,
            vec![
                store
                    .directory
                    .join(format!("recovery-{orphan_digest}.blob"))
            ]
        );
        assert_eq!(
            store.load_recovery_blob(&first_digest).unwrap(),
            first_bytes
        );
        assert_eq!(
            store.load_recovery_blob(&second_digest).unwrap(),
            second_bytes
        );

        store.write_generation("repo-a", 2, &session()).unwrap();
        store.prune_generations("repo-a", 1).unwrap();
        let removed = store.prune_recovery_blobs().unwrap();
        assert_eq!(
            removed,
            vec![
                store
                    .directory
                    .join(format!("recovery-{first_digest}.blob"))
            ]
        );
        assert!(store.load_recovery_blob(&first_digest).is_err());
        assert_eq!(
            store.load_recovery_blob(&second_digest).unwrap(),
            second_bytes
        );
    }

    #[test]
    fn invalid_keys_and_item_identity_mismatches_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        assert!(store.write_generation("../escape", 1, &session()).is_err());

        let mut invalid = session();
        invalid.items.remove(&ItemId(3));
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn editor_presentation_state_is_validated_and_backward_compatible() {
        let baseline = session();
        let baseline_json = serde_json::to_value(&baseline).unwrap();
        for item in baseline_json["items"].as_object().unwrap().values() {
            assert!(item.get("sources").is_none());
            assert!(item.get("soft_wrap").is_none());
        }

        let mut presented = baseline.clone();
        let item = presented.items.get_mut(&ItemId(1)).unwrap();
        item.soft_wrap = true;
        item.folded_ranges.push((
            SessionPoint { row: 2, column: 1 },
            SessionPoint { row: 5, column: 0 },
        ));
        presented.validate().unwrap();
        let encoded = serde_json::to_vec(&presented).unwrap();
        assert_eq!(
            serde_json::from_slice::<WorkspaceSession>(&encoded).unwrap(),
            presented
        );

        let item = presented.items.get_mut(&ItemId(1)).unwrap();
        item.folded_ranges[0] = (
            SessionPoint { row: 5, column: 0 },
            SessionPoint { row: 2, column: 1 },
        );
        assert!(presented.validate().is_err());
    }

    #[test]
    fn pruning_retains_explicit_newest_valid_generations() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        for generation in 1..=4 {
            let mut current = session();
            current
                .workspace
                .focus_item(ItemId((generation % 3) + 1))
                .unwrap();
            store
                .write_generation("repo", generation, &current)
                .unwrap();
        }
        let removed = store.prune_generations("repo", 2).unwrap();
        assert_eq!(removed.len(), 2);
        assert_eq!(store.load_latest("repo").unwrap().unwrap().generation, 4);
        let generation_files = fs::read_dir(&store.directory)
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("repo-") && name.ends_with(".json"))
                    .then_some(entry.path())
            })
            .count();
        assert_eq!(generation_files, 2);
    }

    #[test]
    fn background_writer_serializes_flushes_and_holds_an_exclusive_lease() {
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        let mut first = session();
        let recovery = b"unsaved workspace bytes\n".to_vec();
        let digest = sha256_hex(&recovery);
        first
            .items
            .get_mut(&ItemId(1))
            .unwrap()
            .dirty_recovery_sha256 = Some(digest.clone());

        let mut writer = SessionWriter::start(store.clone(), "repo".to_owned()).unwrap();
        assert!(
            writer
                .try_commit(first.clone(), BTreeMap::from([(digest, recovery)]))
                .unwrap()
        );
        assert!(SessionWriter::start(store.clone(), "repo".to_owned()).is_err());

        let mut final_session = first;
        final_session.workspace.focus_item(ItemId(2)).unwrap();
        let results = writer
            .finish(Some((final_session.clone(), BTreeMap::new())))
            .unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|result| result.result.is_ok()));
        let loaded = store.load_latest("repo").unwrap().unwrap();
        assert_eq!(loaded.generation, 2);
        assert_eq!(loaded.session, final_session);

        let replacement = SessionWriter::start(store, "repo".to_owned()).unwrap();
        assert!(replacement.finish(None).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_generation_and_blob_are_rejected() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::new(directory.path().join("sessions"));
        store.prepare_directory().unwrap();
        let outside = directory.path().join("outside");
        fs::write(&outside, b"{}").unwrap();
        let generation =
            store
                .directory
                .join(format!("repo-{:020}-{}.json", 1, "0".repeat(HASH_HEX_LEN)));
        symlink(&outside, &generation).unwrap();
        assert!(store.load_latest("repo").is_err());

        let digest = "1".repeat(HASH_HEX_LEN);
        symlink(
            &outside,
            store.directory.join(format!("recovery-{digest}.blob")),
        )
        .unwrap();
        assert!(store.load_recovery_blob(&digest).is_err());
    }
}
