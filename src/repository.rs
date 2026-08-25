//! Repository-scoped discovery and search coordination.
//!
//! This module deliberately does not own file text, selections, undo state, or
//! dirty state. Zed's worktree supplies the file set and ignore decisions,
//! `BufferStore` supplies authoritative buffer snapshots and anchor ranges and
//! remains the authority used by the caller to open a selected result. The
//! types here only provide the CLI-specific root identity, deterministic
//! presentation, symlink-alias coalescing, and latest-request reducer.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    fmt,
    hash::{Hash, Hasher},
    io::{self, BufRead as _, BufReader, ErrorKind, Read},
    ops::Range,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
};

use anyhow::{Context as _, Result, bail, ensure};
use gpui::{App, AppContext as _, AsyncApp, Entity, Task};
use language::{Buffer, BufferSnapshot, ByteContent, Point};
use project::{
    ProjectPath, WorktreeId,
    buffer_store::BufferStore,
    search::{MatchPositionHint, SearchQuery},
};
use worktree::decode_byte_header;
use zed_fs::Fs;

/// Alpha 1's fixed number of project-search rows shown to the user.
pub const PROJECT_SEARCH_DISPLAY_LIMIT: usize = 100;

/// The pipeline never creates more workers than GPUI advertises CPUs, and
/// never more than this small UI-search ceiling.
const MAX_PROJECT_SEARCH_WORKERS: usize = 4;
const PROJECT_SEARCH_QUEUE_PER_WORKER: usize = 2;
const PROJECT_SEARCH_SNAPSHOT_CHUNK_BYTES: usize = 8 * 1024;
const PROJECT_SEARCH_SOURCE_FILE_LIMIT: usize = 5_000;
const PROJECT_SEARCH_SOURCE_RANGE_LIMIT: usize = 10_000;

/// One explicit repository root.
///
/// Equality and hashing use the canonical path. Consequently `repo`,
/// `repo/.`, an absolute spelling, and a symlink to `repo` are one identity
/// while `requested_path` remains available for a useful label.
#[derive(Clone, Debug)]
pub struct RepositoryRoot {
    requested_path: Arc<Path>,
    canonical_path: Arc<Path>,
}

impl RepositoryRoot {
    /// Resolve and validate an explicit directory through Zed's filesystem.
    pub async fn open(path: impl AsRef<Path>, fs: &dyn Fs) -> Result<Self> {
        let path = path.as_ref();
        let requested_path = std::path::absolute(path).with_context(|| {
            format!("could not make repository root {} absolute", path.display())
        })?;
        let canonical_path = fs.canonicalize(&requested_path).await.with_context(|| {
            format!(
                "could not canonicalize repository root {}",
                requested_path.display()
            )
        })?;
        let metadata = fs
            .metadata(&canonical_path)
            .await
            .with_context(|| {
                format!(
                    "could not inspect repository root {}",
                    canonical_path.display()
                )
            })?
            .with_context(|| {
                format!(
                    "repository root does not exist: {}",
                    requested_path.display()
                )
            })?;
        ensure!(
            metadata.is_dir,
            "repository root is not a directory: {}",
            requested_path.display()
        );

        Self::from_paths(requested_path, canonical_path)
    }

    fn from_paths(requested_path: PathBuf, canonical_path: PathBuf) -> Result<Self> {
        ensure!(
            requested_path.is_absolute(),
            "requested repository root must be absolute"
        );
        ensure!(
            canonical_path.is_absolute(),
            "canonical repository root must be absolute"
        );
        Ok(Self {
            requested_path: requested_path.into(),
            canonical_path: canonical_path.into(),
        })
    }

    pub fn requested_path(&self) -> &Path {
        &self.requested_path
    }

    /// The stable repository identity.
    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    pub fn label(&self) -> String {
        self.canonical_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.canonical_path.display().to_string())
    }
}

impl PartialEq for RepositoryRoot {
    fn eq(&self, other: &Self) -> bool {
        self.canonical_path == other.canonical_path
    }
}

impl Eq for RepositoryRoot {}

impl Hash for RepositoryRoot {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.canonical_path.hash(state);
    }
}

/// A file entry obtained from a Zed worktree snapshot.
///
/// Production entries come from the completed Zed worktree snapshot. Test-only
/// builder methods support deterministic unit fixtures.
#[derive(Clone, Debug)]
pub struct RepositoryEntry {
    relative_path: String,
    canonical_path: Option<PathBuf>,
    ignored: bool,
    always_included: bool,
    external: bool,
    is_fifo: bool,
    project_path: Option<ProjectPath>,
}

impl RepositoryEntry {
    #[cfg(test)]
    pub fn file(relative_path: impl Into<String>, _size: u64) -> Self {
        Self {
            relative_path: relative_path.into(),
            canonical_path: None,
            ignored: false,
            always_included: false,
            external: false,
            is_fifo: false,
            project_path: None,
        }
    }

    #[cfg(test)]
    pub fn with_canonical_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.canonical_path = Some(path.into());
        self
    }

    #[cfg(test)]
    pub fn ignored(mut self, ignored: bool) -> Self {
        self.ignored = ignored;
        self
    }

    #[cfg(test)]
    pub fn always_included(mut self, always_included: bool) -> Self {
        self.always_included = always_included;
        self
    }

    #[cfg(test)]
    pub fn external(mut self, external: bool) -> Self {
        self.external = external;
        self
    }

    #[cfg(test)]
    pub fn fifo(mut self, is_fifo: bool) -> Self {
        self.is_fifo = is_fifo;
        self
    }
}

#[derive(Clone, Debug)]
struct CandidateFile {
    relative_path: String,
    canonical_path: PathBuf,
    is_symlink_alias: bool,
    project_path: Option<ProjectPath>,
}

/// One physical file as presented by quick-open and project search.
#[derive(Clone, Debug)]
pub struct IndexedFile {
    relative_path: Arc<str>,
    canonical_path: Arc<Path>,
    aliases: Arc<[Arc<str>]>,
    project_path: Option<ProjectPath>,
}

impl IndexedFile {
    pub fn relative_path(&self) -> &str {
        &self.relative_path
    }

    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    /// Every worktree-relative spelling that resolves to this file.
    pub fn aliases(&self) -> &[Arc<str>] {
        &self.aliases
    }

    /// The representative path supplied by Zed's worktree.
    pub fn project_path(&self) -> Option<&ProjectPath> {
        self.project_path.as_ref()
    }
}

/// A complete, deterministic snapshot of searchable files in one worktree.
#[derive(Clone, Debug)]
pub struct RepositoryIndex {
    worktree_id: Option<WorktreeId>,
    files: Vec<IndexedFile>,
    alias_to_file: BTreeMap<String, usize>,
    identity_to_file: BTreeMap<PathBuf, usize>,
}

impl RepositoryIndex {
    /// Build an index only after Zed has completed the current worktree scan.
    /// Ignored and external entries are filtered using the worktree's own
    /// decisions; this function does not walk the filesystem itself.
    pub fn from_worktree(root: RepositoryRoot, worktree: &project::Worktree) -> Result<Self> {
        ensure!(
            worktree.completed_scan_id() >= worktree.scan_id(),
            "cannot index repository before the Zed worktree scan completes"
        );
        let snapshot = worktree.snapshot();
        let entries = snapshot.files(false, 0).map(|entry| RepositoryEntry {
            relative_path: entry.path.as_unix_str().to_owned(),
            canonical_path: entry
                .canonical_path
                .as_ref()
                .map(|path| path.as_ref().to_path_buf()),
            ignored: entry.is_ignored,
            always_included: entry.is_always_included,
            external: entry.is_external,
            is_fifo: entry.is_fifo,
            project_path: Some(ProjectPath {
                worktree_id: snapshot.id(),
                path: entry.path.clone(),
            }),
        });
        Self::from_entries(root, entries)
    }

    /// Build from an already-scanned entry stream. This is also the seam used
    /// by the deterministic Alpha 1 controlled provider.
    pub fn from_entries(
        root: RepositoryRoot,
        entries: impl IntoIterator<Item = RepositoryEntry>,
    ) -> Result<Self> {
        let mut by_identity: BTreeMap<PathBuf, Vec<CandidateFile>> = BTreeMap::new();
        let mut worktree_id = None;

        for entry in entries {
            validate_relative_path(&entry.relative_path)?;
            if let Some(project_path) = &entry.project_path {
                if let Some(existing) = worktree_id {
                    ensure!(
                        existing == project_path.worktree_id,
                        "repository index cannot contain more than one Zed worktree"
                    );
                } else {
                    worktree_id = Some(project_path.worktree_id);
                }
            }
            if (entry.ignored && !entry.always_included) || entry.external || entry.is_fifo {
                continue;
            }

            let is_symlink_alias = entry.canonical_path.is_some();
            let canonical_path = entry
                .canonical_path
                .unwrap_or_else(|| root.canonical_path.join(Path::new(&entry.relative_path)));
            if !canonical_path.is_absolute() {
                bail!(
                    "canonical file path must be absolute: {}",
                    canonical_path.display()
                );
            }
            // A symlink may be visible in the worktree while resolving outside
            // it. Such entries are never repository search candidates.
            if !canonical_path.starts_with(root.canonical_path()) {
                continue;
            }

            by_identity
                .entry(canonical_path.clone())
                .or_default()
                .push(CandidateFile {
                    relative_path: entry.relative_path,
                    canonical_path,
                    is_symlink_alias,
                    project_path: entry.project_path,
                });
        }

        let mut files = Vec::with_capacity(by_identity.len());
        for (_, mut candidates) in by_identity {
            candidates.sort_by(|left, right| {
                left.is_symlink_alias
                    .cmp(&right.is_symlink_alias)
                    .then_with(|| left.relative_path.cmp(&right.relative_path))
            });
            let representative = candidates
                .first()
                .expect("identity groups are constructed from at least one candidate");
            let aliases = candidates
                .iter()
                .map(|candidate| candidate.relative_path.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .map(Arc::<str>::from)
                .collect::<Vec<_>>();
            files.push(IndexedFile {
                relative_path: Arc::from(representative.relative_path.as_str()),
                canonical_path: representative.canonical_path.clone().into(),
                aliases: aliases.clone().into(),
                project_path: representative.project_path.clone(),
            });
        }

        files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        let mut alias_to_file = BTreeMap::new();
        let mut identity_to_file = BTreeMap::new();
        for (index, file) in files.iter().enumerate() {
            identity_to_file.insert(file.canonical_path().to_path_buf(), index);
            for alias in file.aliases() {
                if alias_to_file.insert(alias.to_string(), index).is_some() {
                    bail!("worktree returned duplicate path alias: {alias}");
                }
            }
        }

        Ok(Self {
            worktree_id,
            files,
            alias_to_file,
            identity_to_file,
        })
    }

    pub fn files(&self) -> &[IndexedFile] {
        &self.files
    }

    pub fn file(&self, index: usize) -> Option<&IndexedFile> {
        self.files.get(index)
    }

    pub fn file_for_alias(&self, relative_path: &str) -> Option<&IndexedFile> {
        self.alias_to_file
            .get(relative_path)
            .and_then(|index| self.files.get(*index))
    }

    pub fn file_for_canonical_path(&self, canonical_path: &Path) -> Option<&IndexedFile> {
        self.identity_to_file
            .get(canonical_path)
            .and_then(|index| self.files.get(*index))
    }

    pub fn file_index_for_project_path(&self, path: &ProjectPath) -> Option<usize> {
        if self
            .worktree_id
            .is_some_and(|worktree_id| worktree_id != path.worktree_id)
        {
            return None;
        }
        self.alias_to_file.get(path.path.as_unix_str()).copied()
    }

    /// Deterministic quick-open ranking. ASCII case is folded, Unicode is left
    /// byte-for-byte/character-for-character unchanged, and ties use the
    /// worktree-relative UTF-8 byte order.
    pub fn quick_open(&self, query: &str, limit: usize) -> Vec<QuickOpenMatch> {
        if limit == 0 {
            return Vec::new();
        }
        let query = query.to_ascii_lowercase();
        let mut matches = self
            .files
            .iter()
            .enumerate()
            .filter_map(|(file_index, file)| {
                let score = if query.is_empty() {
                    quick_open_score(&query, file.relative_path())
                } else {
                    file.aliases()
                        .iter()
                        .filter_map(|alias| quick_open_score(&query, alias))
                        .min()
                };
                score.map(|score| QuickOpenMatch { file_index, score })
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            left.score.cmp(&right.score).then_with(|| {
                self.files[left.file_index]
                    .relative_path
                    .cmp(&self.files[right.file_index].relative_path)
            })
        });
        matches.truncate(limit);
        matches
    }
}

fn validate_relative_path(path: &str) -> Result<()> {
    ensure!(!path.is_empty(), "repository file path cannot be empty");
    ensure!(!path.contains('\0'), "repository file path contains NUL");
    let path = Path::new(path);
    ensure!(!path.is_absolute(), "repository file path must be relative");
    ensure!(
        path.components()
            .all(|component| matches!(component, Component::Normal(_))),
        "repository file path is not normalized: {}",
        path.display()
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct QuickOpenScore {
    class: u8,
    gaps: usize,
    start: usize,
    path_len: usize,
}

/// An index into the immutable [`RepositoryIndex`] plus its stable rank.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuickOpenMatch {
    file_index: usize,
    score: QuickOpenScore,
}

impl QuickOpenMatch {
    pub fn file_index(self) -> usize {
        self.file_index
    }
}

fn quick_open_score(folded_query: &str, path: &str) -> Option<QuickOpenScore> {
    let folded_path = path.to_ascii_lowercase();
    let file_name = folded_path.rsplit('/').next().unwrap_or(&folded_path);
    let path_len = folded_path.chars().count();
    if folded_query.is_empty() {
        return Some(QuickOpenScore {
            class: 6,
            gaps: 0,
            start: 0,
            path_len,
        });
    }
    if folded_path == folded_query {
        return Some(QuickOpenScore {
            class: 0,
            gaps: 0,
            start: 0,
            path_len,
        });
    }
    if file_name == folded_query {
        return Some(QuickOpenScore {
            class: 1,
            gaps: 0,
            start: path_len.saturating_sub(file_name.chars().count()),
            path_len,
        });
    }
    if file_name.starts_with(folded_query) {
        return Some(QuickOpenScore {
            class: 2,
            gaps: 0,
            start: path_len.saturating_sub(file_name.chars().count()),
            path_len,
        });
    }
    if let Some(byte_start) = folded_path.find(folded_query) {
        return Some(QuickOpenScore {
            class: 3,
            gaps: 0,
            start: folded_path[..byte_start].chars().count(),
            path_len,
        });
    }

    let query_chars = folded_query.chars().collect::<Vec<_>>();
    let mut next_query = 0;
    let mut first = None;
    for (index, candidate) in folded_path.chars().enumerate() {
        if query_chars.get(next_query) == Some(&candidate) {
            first.get_or_insert(index);
            next_query += 1;
            if next_query == query_chars.len() {
                let first = first.expect("a non-empty subsequence has a first character");
                return Some(QuickOpenScore {
                    class: 4,
                    gaps: index.saturating_sub(first) + 1 - query_chars.len(),
                    start: first,
                    path_len,
                });
            }
        }
    }
    None
}

/// Start one case-sensitive, literal, non-ignored project search.
///
/// The immutable repository index supplies Zed's already-scanned file set and
/// ignore decisions. Open buffers bypass disk prefiltering so dirty text stays
/// authoritative. Closed files are prefiltered through zed_fs, then opened
/// through BufferStore; final matches always come from a Zed BufferSnapshot
/// and are retained as Zed anchor ranges.
///
/// Superseding callers signal cancellation, then retain the running task until
/// collect has naturally joined the fixed worker set. No Zed/GPUI task is
/// dropped to cancel a request.
pub fn start_literal_project_search(
    query: impl Into<String>,
    repository: Arc<RepositoryIndex>,
    fs: Arc<dyn Fs>,
    buffer_store: Entity<BufferStore>,
    open_buffers: Vec<Entity<Buffer>>,
    cx: &mut App,
) -> Result<RunningLiteralSearch> {
    let query = query.into();
    ensure!(!query.is_empty(), "project search query cannot be empty");
    ensure!(
        !query.contains(['\r', '\n']),
        "project search query must fit on one prompt line"
    );
    let zed_query = Arc::new(SearchQuery::text(
        &query,
        false,
        true,
        false,
        Default::default(),
        Default::default(),
        false,
        None,
    )?);
    let cancellation = LiteralSearchCancellation::new();

    // Only caller-declared repository documents may bypass disk prefiltering.
    // BufferStore also contains non-searchable scratch buffers (including
    // file-backed Save As buffers), which must not become project authority.
    let mut open_buffers_by_file: BTreeMap<usize, Vec<OpenLiteralSearchBuffer>> = BTreeMap::new();
    for buffer in open_buffers {
        let metadata = (|| {
            let buffer_ref = buffer.read(cx);
            let file = buffer_ref.file()?;
            if file.disk_state().is_deleted() {
                return None;
            }
            let project_path = ProjectPath::from_file(file.as_ref(), cx);
            let file_index = repository.file_index_for_project_path(&project_path)?;
            let indexed_file = &repository.files()[file_index];
            Some((
                file_index,
                OpenLiteralSearchBuffer {
                    buffer: buffer.clone(),
                    buffer_id: buffer_ref.remote_id(),
                    actual_path: Arc::from(project_path.path.as_unix_str()),
                    actual_is_representative: indexed_file
                        .project_path()
                        .is_some_and(|representative| representative == &project_path),
                },
            ))
        })();
        if let Some((file_index, metadata)) = metadata {
            open_buffers_by_file
                .entry(file_index)
                .or_default()
                .push(metadata);
        }
    }
    for buffers in open_buffers_by_file.values_mut() {
        order_open_literal_search_buffers(buffers);
    }

    let mut work = Vec::with_capacity(repository.files().len());
    for (file_index, file) in repository.files().iter().enumerate() {
        let Some(project_path) = file.project_path().cloned() else {
            continue;
        };
        work.push(LiteralSearchWork {
            ordinal: work.len(),
            project_path,
            canonical_path: file.canonical_path().to_path_buf(),
            open_buffers: open_buffers_by_file
                .remove(&file_index)
                .unwrap_or_default()
                .into_iter()
                .map(|open| open.buffer)
                .collect(),
        });
    }

    let total_work = work.len();
    let worker_count = project_search_worker_count(cx.background_executor().num_cpus());
    let window = worker_count
        .saturating_mul(PROJECT_SEARCH_QUEUE_PER_WORKER)
        .max(1);
    let (work_tx, work_rx) = async_channel::bounded(window);
    let (authority_tx, authority_rx) = async_channel::bounded(window);
    let (matches_tx, matches_rx) = async_channel::bounded(window);
    let (permit_tx, permit_rx) = async_channel::bounded(window);
    for _ in 0..window {
        permit_tx
            .try_send(())
            .expect("fresh project-search permit queue has capacity");
    }

    let internal_stop = LiteralSearchCancellation::new();
    let control = LiteralSearchRunControl {
        user: cancellation.clone(),
        internal: internal_stop.clone(),
    };
    let producer_control = control.clone();
    let producer = cx.background_executor().spawn(async move {
        for item in work {
            if recv_unless_stopped(&permit_rx, &producer_control)
                .await
                .is_none()
            {
                break;
            }
            if !send_unless_stopped(&work_tx, item, &producer_control).await {
                break;
            }
        }
    });

    // Disk prefiltering stays entirely on the background executor. In
    // particular, a no-match file never wakes an AsyncApp task.
    let mut prefilter_workers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let work_rx = work_rx.clone();
        let authority_tx = authority_tx.clone();
        let matches_tx = matches_tx.clone();
        let query = zed_query.clone();
        let fs = fs.clone();
        let worker_control = control.clone();
        prefilter_workers.push(cx.background_executor().spawn(async move {
            run_literal_prefilter_worker(
                work_rx,
                authority_tx,
                matches_tx,
                query,
                fs,
                worker_control,
            )
            .await;
        }));
    }
    drop(work_rx);
    drop(authority_tx);

    // AsyncApp is needed only once disk prefiltering found a match, or when an
    // already-open buffer is authoritative. Fixed consumers keep this stage
    // bounded without one foreground/background spawn round trip per file.
    let mut authority_workers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let authority_rx = authority_rx.clone();
        let matches_tx = matches_tx.clone();
        let query = zed_query.clone();
        let buffer_store = buffer_store.clone();
        let repository = repository.clone();
        let worker_control = control.clone();
        authority_workers.push(cx.spawn(async move |cx| {
            run_literal_authority_worker(
                authority_rx,
                matches_tx,
                query,
                buffer_store,
                repository,
                worker_control,
                cx,
            )
            .await;
        }));
    }
    drop(authority_rx);
    drop(matches_tx);

    let task_cancellation = cancellation.clone();
    let query_for_output: Arc<str> = query.into();
    let collector = cx.background_executor().spawn(async move {
        let mut candidates = OrderedSearchCandidates::default();
        let mut ordered = OrderedFileResults::default();
        let mut terminal_reached = false;
        let mut failure = None;
        let mut retired = 0usize;

        while let Ok(worker_result) = matches_rx.recv().await {
            if terminal_reached {
                continue;
            }
            let ordinal = worker_result.ordinal;
            for result in ordered.insert(ordinal, worker_result) {
                retired = retired.saturating_add(1);
                match candidates.push(result) {
                    OrderedSearchDisposition::Continue => {
                        let _ = permit_tx.try_send(());
                    }
                    OrderedSearchDisposition::SourceLimit => {
                        terminal_reached = true;
                        internal_stop.cancel();
                        ordered.clear();
                        break;
                    }
                    OrderedSearchDisposition::Error(error) => {
                        terminal_reached = true;
                        failure = Some(error);
                        internal_stop.cancel();
                        ordered.clear();
                        break;
                    }
                }
            }
        }

        if task_cancellation.is_cancelled() {
            bail!("project search cancelled");
        }
        if let Some(error) = failure {
            return Err(error);
        }
        if !terminal_reached {
            ensure!(
                retired == total_work,
                "project search pipeline retired {retired} of {total_work} files"
            );
        }
        Ok(candidates.finish(query_for_output))
    });

    let task = cx.spawn(async move |_cx| {
        // Fixed tasks are always allowed to finish and are explicitly joined.
        producer.await;
        for worker in prefilter_workers {
            worker.await;
        }
        for worker in authority_workers {
            worker.await;
        }
        collector.await
    });

    Ok(RunningLiteralSearch { task, cancellation })
}

fn project_search_worker_count(num_cpus: usize) -> usize {
    num_cpus.clamp(1, MAX_PROJECT_SEARCH_WORKERS)
}

struct OpenLiteralSearchBuffer {
    buffer: Entity<Buffer>,
    buffer_id: text::BufferId,
    actual_path: Arc<str>,
    actual_is_representative: bool,
}

fn order_open_literal_search_buffers(buffers: &mut Vec<OpenLiteralSearchBuffer>) {
    buffers.sort_by(|left, right| {
        right
            .actual_is_representative
            .cmp(&left.actual_is_representative)
            .then_with(|| left.actual_path.cmp(&right.actual_path))
            .then_with(|| left.buffer_id.cmp(&right.buffer_id))
    });
    buffers.dedup_by_key(|buffer| buffer.buffer_id);
}

#[derive(Clone)]
struct LiteralSearchWork {
    ordinal: usize,
    project_path: ProjectPath,
    canonical_path: PathBuf,
    open_buffers: Vec<Entity<Buffer>>,
}

struct LiteralSearchAuthorityWork {
    ordinal: usize,
    project_path: ProjectPath,
    open_buffers: Vec<Entity<Buffer>>,
    hint: MatchPositionHint,
}

struct LiteralSearchWorkerResult {
    ordinal: usize,
    candidates: Vec<SearchCandidate>,
    source_limit_reached: bool,
    error: Option<anyhow::Error>,
}

impl LiteralSearchWorkerResult {
    fn empty(ordinal: usize) -> Self {
        Self {
            ordinal,
            candidates: Vec::new(),
            source_limit_reached: false,
            error: None,
        }
    }

    fn success(
        ordinal: usize,
        candidates: Vec<SearchCandidate>,
        source_limit_reached: bool,
    ) -> Self {
        Self {
            ordinal,
            candidates,
            source_limit_reached,
            error: None,
        }
    }

    fn failure(ordinal: usize, error: anyhow::Error) -> Self {
        Self {
            ordinal,
            candidates: Vec::new(),
            source_limit_reached: false,
            error: Some(error),
        }
    }
}

struct OrderedFileResults<T> {
    pending: BTreeMap<usize, T>,
    next_ordinal: usize,
}

impl<T> Default for OrderedFileResults<T> {
    fn default() -> Self {
        Self {
            pending: BTreeMap::new(),
            next_ordinal: 0,
        }
    }
}

impl<T> OrderedFileResults<T> {
    fn insert(&mut self, ordinal: usize, result: T) -> Vec<T> {
        let replaced = self.pending.insert(ordinal, result);
        debug_assert!(replaced.is_none(), "project search file completed twice");
        let mut ready = Vec::new();
        while let Some(result) = self.pending.remove(&self.next_ordinal) {
            self.next_ordinal = self.next_ordinal.saturating_add(1);
            ready.push(result);
        }
        ready
    }

    fn clear(&mut self) {
        self.pending.clear();
    }
}

#[derive(Default)]
struct ProjectSearchSourceBudget {
    matched_files: usize,
    ranges: usize,
    source_limit_reached: bool,
}

impl ProjectSearchSourceBudget {
    /// Returns the deterministic prefix length to retain and whether scanning
    /// must stop. Exact limits are accepted; only proof of an additional file
    /// or range sets the lower-bound flag.
    fn admit(&mut self, ranges: usize, worker_limit_reached: bool) -> (usize, bool) {
        if ranges == 0 {
            self.source_limit_reached |= worker_limit_reached;
            return (0, self.source_limit_reached);
        }
        if self.matched_files == PROJECT_SEARCH_SOURCE_FILE_LIMIT {
            self.source_limit_reached = true;
            return (0, true);
        }
        self.matched_files += 1;

        let retained = ranges.min(PROJECT_SEARCH_SOURCE_RANGE_LIMIT.saturating_sub(self.ranges));
        self.ranges += retained;
        self.source_limit_reached |= retained < ranges || worker_limit_reached;
        (retained, self.source_limit_reached)
    }
}

#[derive(Default)]
struct OrderedSearchCandidates {
    candidates: Vec<SearchCandidate>,
    budget: ProjectSearchSourceBudget,
}

enum OrderedSearchDisposition {
    Continue,
    SourceLimit,
    Error(anyhow::Error),
}

impl OrderedSearchCandidates {
    /// Consume exactly one file result in repository order. The first error or
    /// proven source-cap excess stops further dispatch after this ordinal.
    fn push(&mut self, mut result: LiteralSearchWorkerResult) -> OrderedSearchDisposition {
        if let Some(error) = result.error.take() {
            return OrderedSearchDisposition::Error(error);
        }
        let (retained, stop) = self
            .budget
            .admit(result.candidates.len(), result.source_limit_reached);
        result.candidates.truncate(retained);
        self.candidates.extend(result.candidates);
        if stop {
            OrderedSearchDisposition::SourceLimit
        } else {
            OrderedSearchDisposition::Continue
        }
    }

    fn finish(self, query: Arc<str>) -> ProjectSearchOutput {
        finalize_literal_search(query, self.candidates, self.budget.source_limit_reached)
    }
}

#[derive(Clone)]
struct LiteralSearchCancellation {
    cancelled: Arc<AtomicBool>,
    sender: async_channel::Sender<()>,
    receiver: async_channel::Receiver<()>,
}

impl LiteralSearchCancellation {
    fn new() -> Self {
        let (sender, receiver) = async_channel::bounded(1);
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            sender,
            receiver,
        }
    }

    fn cancel(&self) {
        if !self.cancelled.swap(true, AtomicOrdering::AcqRel) {
            self.sender.close();
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(AtomicOrdering::Acquire)
    }

    async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let _ = self.receiver.recv().await;
    }
}

#[derive(Clone)]
struct LiteralSearchRunControl {
    user: LiteralSearchCancellation,
    internal: LiteralSearchCancellation,
}

impl LiteralSearchRunControl {
    fn is_stopped(&self) -> bool {
        self.user.is_cancelled() || self.internal.is_cancelled()
    }

    async fn stopped(&self) {
        use futures::{FutureExt as _, pin_mut, select_biased};

        let user = self.user.cancelled().fuse();
        let internal = self.internal.cancelled().fuse();
        pin_mut!(user, internal);
        select_biased! {
            _ = user => {},
            _ = internal => {},
        }
    }
}

async fn send_unless_stopped<T>(
    sender: &async_channel::Sender<T>,
    value: T,
    control: &LiteralSearchRunControl,
) -> bool {
    use futures::{FutureExt as _, pin_mut, select_biased};

    let send = sender.send(value).fuse();
    let stopped = control.stopped().fuse();
    pin_mut!(send, stopped);
    select_biased! {
        _ = stopped => false,
        result = send => result.is_ok(),
    }
}

async fn recv_unless_stopped<T>(
    receiver: &async_channel::Receiver<T>,
    control: &LiteralSearchRunControl,
) -> Option<T> {
    use futures::{FutureExt as _, pin_mut, select_biased};

    let recv = receiver.recv().fuse();
    let stopped = control.stopped().fuse();
    pin_mut!(recv, stopped);
    select_biased! {
        _ = stopped => None,
        result = recv => result.ok(),
    }
}

async fn complete_unless_stopped<F>(
    future: F,
    control: &LiteralSearchRunControl,
) -> Option<F::Output>
where
    F: std::future::Future,
{
    use futures::{FutureExt as _, pin_mut, select_biased};

    let future = future.fuse();
    let stopped = control.stopped().fuse();
    pin_mut!(future, stopped);
    select_biased! {
        _ = stopped => None,
        result = future => Some(result),
    }
}

async fn run_literal_prefilter_worker(
    work_rx: async_channel::Receiver<LiteralSearchWork>,
    authority_tx: async_channel::Sender<LiteralSearchAuthorityWork>,
    matches_tx: async_channel::Sender<LiteralSearchWorkerResult>,
    query: Arc<SearchQuery>,
    fs: Arc<dyn Fs>,
    control: LiteralSearchRunControl,
) {
    while let Some(work) = recv_unless_stopped(&work_rx, &control).await {
        let LiteralSearchWork {
            ordinal,
            project_path,
            canonical_path,
            open_buffers,
        } = work;
        let hint = if open_buffers.is_empty() {
            match detect_literal_candidate(
                query.clone(),
                fs.clone(),
                canonical_path.clone(),
                control.clone(),
            )
            .await
            {
                Ok(hint) => hint,
                Err(error) => {
                    if control.is_stopped() {
                        break;
                    }
                    let error = error.context(format!(
                        "could not prefilter project-search file {}",
                        canonical_path.display()
                    ));
                    if !send_unless_stopped(
                        &matches_tx,
                        LiteralSearchWorkerResult::failure(ordinal, error),
                        &control,
                    )
                    .await
                    {
                        break;
                    }
                    continue;
                }
            }
        } else {
            Some(MatchPositionHint::default())
        };

        let Some(hint) = hint else {
            if control.is_stopped()
                || !send_unless_stopped(
                    &matches_tx,
                    LiteralSearchWorkerResult::empty(ordinal),
                    &control,
                )
                .await
            {
                break;
            }
            continue;
        };
        if control.is_stopped()
            || !send_unless_stopped(
                &authority_tx,
                LiteralSearchAuthorityWork {
                    ordinal,
                    project_path,
                    open_buffers,
                    hint,
                },
                &control,
            )
            .await
        {
            break;
        }
    }
}

async fn run_literal_authority_worker(
    authority_rx: async_channel::Receiver<LiteralSearchAuthorityWork>,
    matches_tx: async_channel::Sender<LiteralSearchWorkerResult>,
    query: Arc<SearchQuery>,
    buffer_store: Entity<BufferStore>,
    repository: Arc<RepositoryIndex>,
    control: LiteralSearchRunControl,
    cx: &mut AsyncApp,
) {
    while let Some(work) = recv_unless_stopped(&authority_rx, &control).await {
        let LiteralSearchAuthorityWork {
            ordinal,
            project_path,
            open_buffers,
            hint,
        } = work;
        let open_buffers_are_authority = !open_buffers.is_empty();
        let buffers = if open_buffers_are_authority {
            open_buffers
        } else {
            let open =
                buffer_store.update(cx, |store, cx| store.open_buffer(project_path.clone(), cx));
            let Some(open) = complete_unless_stopped(open, &control).await else {
                break;
            };
            match open {
                Ok(buffer) => vec![buffer],
                Err(error) if error_has_io_kind(&error, ErrorKind::NotFound) => {
                    if !send_unless_stopped(
                        &matches_tx,
                        LiteralSearchWorkerResult::empty(ordinal),
                        &control,
                    )
                    .await
                    {
                        break;
                    }
                    continue;
                }
                Err(error) => {
                    if control.is_stopped() {
                        break;
                    }
                    let error = error.context(format!(
                        "could not open project-search buffer {}",
                        project_path.path.as_unix_str()
                    ));
                    if !send_unless_stopped(
                        &matches_tx,
                        LiteralSearchWorkerResult::failure(ordinal, error),
                        &control,
                    )
                    .await
                    {
                        break;
                    }
                    continue;
                }
            }
        };

        // One physical repository file may have multiple open alias buffers.
        // Search every authoritative snapshot, then retain the deterministic
        // representative for each physical byte range without exceeding the
        // per-file range bound at any point.
        let mut candidates_by_range = BTreeMap::new();
        let mut source_limit_reached = false;
        for buffer in buffers {
            if control.is_stopped() {
                break;
            }
            let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
            let find_matches = cx.background_spawn(search_snapshot_in_chunks(
                query.clone(),
                snapshot,
                hint,
                control.clone(),
            ));
            let snapshot_matches = find_matches.await;
            if control.is_stopped() {
                break;
            }

            let buffer_for_hits = buffer.clone();
            let projected = buffer.read_with(cx, |buffer, cx| {
                project_buffer_matches(
                    repository.as_ref(),
                    buffer_for_hits,
                    buffer,
                    snapshot_matches.ranges,
                    cx,
                )
            });
            for candidate in projected {
                let key = (
                    candidate.identity.clone(),
                    candidate.byte_range.start,
                    candidate.byte_range.end,
                );
                if let Some(existing) = candidates_by_range.get_mut(&key) {
                    if compare_search_candidates(&candidate, existing).is_lt() {
                        *existing = candidate;
                    }
                } else if candidates_by_range.len() == PROJECT_SEARCH_SOURCE_RANGE_LIMIT {
                    source_limit_reached = true;
                    break;
                } else {
                    candidates_by_range.insert(key, candidate);
                }
            }
            source_limit_reached |= snapshot_matches.source_limit_reached;
            if source_limit_reached {
                break;
            }
        }
        if control.is_stopped() {
            break;
        }

        let mut candidates = candidates_by_range.into_values().collect::<Vec<_>>();
        candidates.sort_by(compare_search_candidates);
        if !send_unless_stopped(
            &matches_tx,
            LiteralSearchWorkerResult::success(ordinal, candidates, source_limit_reached),
            &control,
        )
        .await
        {
            break;
        }
    }
}

/// A reader that turns an atomic cancellation request into a non-retryable
/// read error. `BufRead` automatically retries `Interrupted`, so cancellation
/// deliberately uses `Other` and returns after at most one bounded read.
struct CancellableReader<R> {
    inner: R,
    control: LiteralSearchRunControl,
}

impl<R> CancellableReader<R> {
    fn new(inner: R, control: LiteralSearchRunControl) -> Self {
        Self { inner, control }
    }
}

impl<R: Read> Read for CancellableReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.control.is_stopped() {
            return Err(io::Error::other("project search stopped"));
        }
        const MAX_READ: usize = 16 * 1024;
        let len = buffer.len().min(MAX_READ);
        self.inner.read(&mut buffer[..len])
    }
}

fn error_has_io_kind(error: &anyhow::Error, kind: ErrorKind) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == kind)
    })
}

async fn detect_literal_candidate(
    query: Arc<SearchQuery>,
    fs: Arc<dyn Fs>,
    path: PathBuf,
    control: LiteralSearchRunControl,
) -> Result<Option<MatchPositionHint>> {
    if control.is_stopped() {
        return Ok(None);
    }
    let Some(file) = complete_unless_stopped(fs.open_sync(&path), &control).await else {
        return Ok(None);
    };
    let file = match file {
        Ok(file) => file,
        Err(error) if error_has_io_kind(&error, ErrorKind::NotFound) => return Ok(None),
        Err(error) => return Err(error),
    };
    let reader: Box<dyn Read + Send + Sync> =
        Box::new(CancellableReader::new(file, control.clone()));
    let mut file = BufReader::new(reader);
    let (has_bom, byte_content, plain_utf8) = {
        let file_start = file.fill_buf()?;
        let (bom_encoding, byte_content) = decode_byte_header(file_start);
        (
            bom_encoding.is_some(),
            byte_content,
            is_utf8_prefix(file_start),
        )
    };
    if byte_content == ByteContent::Binary {
        return Ok(None);
    }

    if !has_bom && byte_content == ByteContent::Unknown && plain_utf8 {
        match query.detect(file).await {
            Ok(hint) => return Ok(hint),
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == ErrorKind::InvalidData)
                    && !control.is_stopped() =>
            {
                return Ok(Some(MatchPositionHint::default()));
            }
            Err(error) => return Err(error),
        }
    }

    // Disk detection is only an optimization. BufferStore owns decoding, so
    // BOM, UTF-16, and other non-plain files proceed to its snapshot authority.
    Ok((!control.is_stopped()).then(MatchPositionHint::default))
}

fn is_utf8_prefix(bytes: &[u8]) -> bool {
    match std::str::from_utf8(bytes) {
        Ok(_) => true,
        Err(error) => error.error_len().is_none(),
    }
}

struct SnapshotSearchMatches {
    ranges: Vec<Range<language::Anchor>>,
    source_limit_reached: bool,
}

async fn search_snapshot_in_chunks(
    query: Arc<SearchQuery>,
    snapshot: BufferSnapshot,
    hint: MatchPositionHint,
    control: LiteralSearchRunControl,
) -> SnapshotSearchMatches {
    search_snapshot_with_chunk_bytes(
        query,
        snapshot,
        hint,
        control,
        PROJECT_SEARCH_SNAPSHOT_CHUNK_BYTES,
    )
    .await
}

/// Search from one global non-overlap cursor. Each primary span is fixed-width
/// and UTF-8 aligned; query_len-1 bytes of lookahead allow SearchQuery to emit
/// a boundary-crossing match. Only matches starting in the primary span are
/// accepted, so the next slice resumes at the whole-search Aho-Corasick phase.
async fn search_snapshot_with_chunk_bytes(
    query: Arc<SearchQuery>,
    snapshot: BufferSnapshot,
    hint: MatchPositionHint,
    control: LiteralSearchRunControl,
    chunk_bytes: usize,
) -> SnapshotSearchMatches {
    let len = snapshot.len();
    let mut chunk_start = match hint {
        MatchPositionHint::Line(line) if line > 0 => {
            snapshot.point_to_offset(Point::new(line.min(snapshot.max_point().row), 0))
        }
        MatchPositionHint::ByteOffset(offset) => {
            snapshot.clip_offset(offset.min(len), text::Bias::Left)
        }
        _ => 0,
    };
    let query_overlap = query.as_str().len().saturating_sub(1);
    let mut byte_ranges = Vec::new();
    let mut source_limit_reached = false;

    while chunk_start < len {
        if control.is_stopped() {
            return SnapshotSearchMatches {
                ranges: Vec::new(),
                source_limit_reached: false,
            };
        }
        let core_end = snapshot_chunk_end(&snapshot, chunk_start, chunk_bytes.max(1));
        let scan_end = snapshot.clip_offset(
            core_end.saturating_add(query_overlap).min(len),
            text::Bias::Right,
        );
        let chunk_matches = query.search(&snapshot, Some(chunk_start..scan_end)).await;
        if control.is_stopped() {
            return SnapshotSearchMatches {
                ranges: Vec::new(),
                source_limit_reached: false,
            };
        }

        let mut last_accepted_end = None;
        for range in chunk_matches {
            let range = (range.start + chunk_start)..(range.end + chunk_start);
            if range.start >= core_end {
                break;
            }
            if byte_ranges.len() == PROJECT_SEARCH_SOURCE_RANGE_LIMIT {
                source_limit_reached = true;
                break;
            }
            last_accepted_end = Some(range.end);
            byte_ranges.push(range);
        }
        if source_limit_reached {
            break;
        }
        chunk_start = core_end.max(last_accepted_end.unwrap_or(core_end));
    }

    byte_ranges.sort_by(|left, right| {
        left.start
            .cmp(&right.start)
            .then_with(|| left.end.cmp(&right.end))
    });
    byte_ranges.dedup();
    let ranges = byte_ranges
        .into_iter()
        .map(|range| snapshot.anchor_before(range.start)..snapshot.anchor_after(range.end))
        .collect();
    SnapshotSearchMatches {
        ranges,
        source_limit_reached,
    }
}

fn snapshot_chunk_end(snapshot: &BufferSnapshot, start: usize, chunk_bytes: usize) -> usize {
    let len = snapshot.len();
    let target = start.saturating_add(chunk_bytes).min(len);
    if target == len {
        len
    } else {
        snapshot.clip_offset(target, text::Bias::Right)
    }
}

fn finalize_literal_search(
    query: Arc<str>,
    mut candidates: Vec<SearchCandidate>,
    source_limit_reached: bool,
) -> ProjectSearchOutput {
    candidates.sort_by(compare_search_candidates);
    let mut seen = BTreeSet::new();
    candidates.retain(|candidate| {
        seen.insert((
            candidate.identity.clone(),
            candidate.byte_range.start,
            candidate.byte_range.end,
        ))
    });
    candidates.sort_by(|left, right| {
        left.summary
            .path
            .cmp(&right.summary.path)
            .then_with(|| left.summary.line.cmp(&right.summary.line))
            .then_with(|| left.summary.column.cmp(&right.summary.column))
            .then_with(|| left.byte_range.start.cmp(&right.byte_range.start))
            .then_with(|| left.byte_range.end.cmp(&right.byte_range.end))
    });

    let total_hits = candidates.len();
    candidates.truncate(PROJECT_SEARCH_DISPLAY_LIMIT);
    let matches = candidates
        .into_iter()
        .map(|candidate| ProjectSearchHit {
            summary: candidate.summary,
            file_index: candidate.file_index,
            buffer: candidate.buffer,
            anchor_range: candidate.anchor_range,
            byte_range: candidate.byte_range,
        })
        .collect();
    ProjectSearchOutput {
        query,
        matches,
        total_hits,
        source_limit_reached,
    }
}

#[must_use = "a running project search must be collected to completion"]
pub struct RunningLiteralSearch {
    task: Task<Result<ProjectSearchOutput>>,
    cancellation: LiteralSearchCancellation,
}

#[derive(Clone)]
pub struct RunningLiteralSearchCancellation {
    cancellation: LiteralSearchCancellation,
}

impl RunningLiteralSearchCancellation {
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }

    #[cfg(test)]
    pub(crate) fn test_probe() -> Self {
        Self {
            cancellation: LiteralSearchCancellation::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}

impl RunningLiteralSearch {
    pub fn cancellation_handle(&self) -> RunningLiteralSearchCancellation {
        RunningLiteralSearchCancellation {
            cancellation: self.cancellation.clone(),
        }
    }

    pub async fn collect(self, _cx: &mut AsyncApp) -> Result<ProjectSearchOutput> {
        self.task.await
    }
}

struct SearchCandidate {
    summary: ProjectSearchSummary,
    file_index: usize,
    identity: PathBuf,
    actual_path: Arc<str>,
    actual_is_representative: bool,
    buffer: Entity<Buffer>,
    anchor_range: Range<language::Anchor>,
    byte_range: Range<usize>,
}

fn compare_search_candidates(left: &SearchCandidate, right: &SearchCandidate) -> Ordering {
    left.identity
        .cmp(&right.identity)
        .then_with(|| left.byte_range.start.cmp(&right.byte_range.start))
        .then_with(|| left.byte_range.end.cmp(&right.byte_range.end))
        .then_with(|| {
            right
                .actual_is_representative
                .cmp(&left.actual_is_representative)
        })
        .then_with(|| left.actual_path.cmp(&right.actual_path))
        .then_with(|| left.summary.path.cmp(&right.summary.path))
}

fn project_buffer_matches(
    repository: &RepositoryIndex,
    entity: Entity<Buffer>,
    buffer: &Buffer,
    ranges: Vec<Range<language::Anchor>>,
    cx: &App,
) -> Vec<SearchCandidate> {
    let Some(file) = buffer.file() else {
        return Vec::new();
    };
    let actual_path = ProjectPath::from_file(file.as_ref(), cx);
    let Some(file_index) = repository.file_index_for_project_path(&actual_path) else {
        return Vec::new();
    };
    let indexed_file = &repository.files[file_index];
    let actual_is_representative = indexed_file
        .project_path()
        .is_some_and(|representative| representative == &actual_path);
    let snapshot = buffer.snapshot();

    ranges
        .into_iter()
        .filter_map(|anchor_range| {
            let start: usize = snapshot.summary_for_anchor(&anchor_range.start);
            let end: usize = snapshot.summary_for_anchor(&anchor_range.end);
            let point = snapshot.offset_to_point(start);
            let line_start = snapshot.point_to_offset(Point::new(point.row, 0));
            let line_end =
                snapshot.point_to_offset(Point::new(point.row, snapshot.line_len(point.row)));
            let preview = snapshot
                .text_for_range(line_start..line_end)
                .collect::<String>();
            let byte_column = start.checked_sub(line_start)?;
            let column = unicode_scalar_column(&preview, byte_column)?;
            Some(SearchCandidate {
                summary: ProjectSearchSummary {
                    path: indexed_file.relative_path.clone(),
                    line: point.row.saturating_add(1),
                    column,
                    preview,
                },
                file_index,
                identity: indexed_file.canonical_path().to_path_buf(),
                actual_path: Arc::from(actual_path.path.as_unix_str()),
                actual_is_representative,
                buffer: entity.clone(),
                anchor_range,
                byte_range: start..end,
            })
        })
        .collect()
}

fn unicode_scalar_column(line: &str, byte_column: usize) -> Option<u32> {
    let prefix = line.get(..byte_column)?;
    u32::try_from(prefix.chars().count()).ok()?.checked_add(1)
}

/// Exact JSON-facing fields for one project-search row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectSearchSummary {
    pub path: Arc<str>,
    pub line: u32,
    pub column: u32,
    pub preview: String,
}

/// Presentation plus the Zed-owned target needed to open and select a match.
#[derive(Clone)]
pub struct ProjectSearchHit {
    pub summary: ProjectSearchSummary,
    pub file_index: usize,
    pub buffer: Entity<Buffer>,
    pub anchor_range: Range<language::Anchor>,
    pub byte_range: Range<usize>,
}

impl fmt::Debug for ProjectSearchHit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProjectSearchHit")
            .field("summary", &self.summary)
            .field("file_index", &self.file_index)
            .field("anchor_range", &self.anchor_range)
            .field("byte_range", &self.byte_range)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug)]
pub struct ProjectSearchOutput {
    pub query: Arc<str>,
    pub matches: Vec<ProjectSearchHit>,
    /// Full de-duplicated count before the fixed display truncation.
    pub total_hits: usize,
    /// Zed's own safety cap was reached, so `total_hits` is a lower bound.
    pub source_limit_reached: bool,
}

/// Opaque token identifying one project-search request.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SearchGeneration(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionDisposition {
    Published,
    DiscardedStale,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LatestSearchState<Q, R, E> {
    Idle,
    Running {
        generation: SearchGeneration,
        query: Q,
    },
    Ready {
        generation: SearchGeneration,
        query: Q,
        result: R,
    },
    Failed {
        generation: SearchGeneration,
        query: Q,
        error: E,
    },
}

/// Single-threaded reducer that makes out-of-order async completion harmless.
/// The GPUI event loop owns it; providers only return a generation with their
/// completion and never mutate UI state directly.
#[derive(Clone, Debug)]
pub struct LatestSearch<Q, R, E> {
    next_generation: u64,
    state: LatestSearchState<Q, R, E>,
}

impl<Q, R, E> Default for LatestSearch<Q, R, E> {
    fn default() -> Self {
        Self {
            next_generation: 0,
            state: LatestSearchState::Idle,
        }
    }
}

impl<Q, R, E> LatestSearch<Q, R, E> {
    pub fn state(&self) -> &LatestSearchState<Q, R, E> {
        &self.state
    }

    pub fn begin(&mut self, query: Q) -> Result<SearchGeneration> {
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .context("project search generation exhausted")?;
        let generation = SearchGeneration(self.next_generation);
        self.state = LatestSearchState::Running { generation, query };
        Ok(generation)
    }

    pub fn cancel(&mut self) {
        self.state = LatestSearchState::Idle;
    }

    pub fn complete(
        &mut self,
        generation: SearchGeneration,
        completion: std::result::Result<R, E>,
    ) -> CompletionDisposition {
        let old_state = std::mem::replace(&mut self.state, LatestSearchState::Idle);
        let LatestSearchState::Running {
            generation: current,
            query,
        } = old_state
        else {
            self.state = old_state;
            return CompletionDisposition::DiscardedStale;
        };
        if current != generation {
            self.state = LatestSearchState::Running {
                generation: current,
                query,
            };
            return CompletionDisposition::DiscardedStale;
        }

        self.state = match completion {
            Ok(result) => LatestSearchState::Ready {
                generation,
                query,
                result,
            },
            Err(error) => LatestSearchState::Failed {
                generation,
                query,
                error,
            },
        };
        CompletionDisposition::Published
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashSet, sync::mpsc};

    fn absolute_test_path(path: &str) -> PathBuf {
        path.trim_start_matches('/').split('/').fold(
            std::env::temp_dir().join("zec-repository-unit"),
            |path, part| path.join(part),
        )
    }

    fn root(requested: &str, canonical: &str) -> RepositoryRoot {
        RepositoryRoot::from_paths(absolute_test_path(requested), absolute_test_path(canonical))
            .unwrap()
    }

    #[test]
    fn root_identity_is_canonical_but_preserves_requested_label() {
        let direct = root("/repo", "/repo");
        let alias = root("/links/project", "/repo");

        assert_eq!(direct, alias);
        assert_eq!(alias.label(), "repo");
        let roots = HashSet::from([direct, alias]);
        assert_eq!(roots.len(), 1);
    }

    #[test]
    fn index_filters_zed_ignore_and_external_decisions() {
        let index = RepositoryIndex::from_entries(
            root("/repo", "/repo"),
            [
                RepositoryEntry::file("src/main.rs", 10),
                RepositoryEntry::file("target/hidden", 1).ignored(true),
                RepositoryEntry::file(".env.example", 2)
                    .ignored(true)
                    .always_included(true),
                RepositoryEntry::file("outside", 3)
                    .with_canonical_path(absolute_test_path("/control/outside"))
                    .external(true),
                RepositoryEntry::file("unmarked-outside", 4)
                    .with_canonical_path(absolute_test_path("/control/unmarked-outside")),
            ],
        )
        .unwrap();

        assert_eq!(
            index
                .files()
                .iter()
                .map(IndexedFile::relative_path)
                .collect::<Vec<_>>(),
            vec![".env.example", "src/main.rs"]
        );
    }

    #[test]
    fn index_excludes_fifo_before_project_search_work_is_built() {
        let index = RepositoryIndex::from_entries(
            root("/repo", "/repo"),
            [
                RepositoryEntry::file("src/main.rs", 10),
                RepositoryEntry::file("events.pipe", 0).fifo(true),
            ],
        )
        .unwrap();

        assert_eq!(
            index
                .files()
                .iter()
                .map(IndexedFile::relative_path)
                .collect::<Vec<_>>(),
            vec!["src/main.rs"]
        );
        assert!(index.file_for_alias("events.pipe").is_none());
    }

    #[test]
    fn symlink_aliases_share_one_identity_and_prefer_the_real_path() {
        let index = RepositoryIndex::from_entries(
            root("/repo", "/repo"),
            [
                RepositoryEntry::file("z-alias.rs", 20)
                    .with_canonical_path(absolute_test_path("/repo/src/real.rs")),
                RepositoryEntry::file("src/real.rs", 20),
                RepositoryEntry::file("a-alias.rs", 20)
                    .with_canonical_path(absolute_test_path("/repo/src/real.rs")),
            ],
        )
        .unwrap();

        assert_eq!(index.files().len(), 1);
        let file = &index.files()[0];
        assert_eq!(file.relative_path(), "src/real.rs");
        assert_eq!(
            file.aliases().iter().map(AsRef::as_ref).collect::<Vec<_>>(),
            vec!["a-alias.rs", "src/real.rs", "z-alias.rs"]
        );
        assert!(std::ptr::eq(
            index.file_for_alias("z-alias.rs").unwrap(),
            index.file_for_alias("src/real.rs").unwrap()
        ));
        assert_eq!(
            index
                .file_for_canonical_path(&absolute_test_path("/repo/src/real.rs"))
                .unwrap()
                .relative_path(),
            "src/real.rs"
        );
        assert_eq!(
            index
                .quick_open("z-alias.rs", 1)
                .first()
                .map(|result| index.files()[result.file_index()].relative_path()),
            Some("src/real.rs")
        );
    }

    #[test]
    fn index_rejects_ambiguous_non_normal_paths() {
        for path in ["", "/absolute", "../escape", "src/../other", "./main.rs"] {
            let error = RepositoryIndex::from_entries(
                root("/repo", "/repo"),
                [RepositoryEntry::file(path, 0)],
            )
            .unwrap_err();
            assert!(!error.to_string().is_empty(), "{path:?} must be rejected");
        }
    }

    #[test]
    fn quick_open_is_ranked_and_ties_are_utf8_byte_deterministic() {
        let index = RepositoryIndex::from_entries(
            root("/repo", "/repo"),
            [
                RepositoryEntry::file("z/日本 語.rs", 1),
                RepositoryEntry::file("a/日本 語.rs", 1),
                RepositoryEntry::file("日本 語.rs.bak", 1),
                RepositoryEntry::file("src/n_h_o.rs", 1),
                RepositoryEntry::file("src/no-hit.rs", 1),
            ],
        )
        .unwrap();

        let matches = index.quick_open("日本 語.rs", 10);
        let paths = matches
            .iter()
            .map(|result| index.files()[result.file_index()].relative_path())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec!["a/日本 語.rs", "z/日本 語.rs", "日本 語.rs.bak"]
        );
        assert_eq!(
            index
                .quick_open("NHO", 1)
                .first()
                .map(|result| index.files()[result.file_index()].relative_path()),
            Some("src/n_h_o.rs")
        );
    }

    #[test]
    fn unicode_columns_are_one_based_scalar_indices() {
        let line = "ab日本 語.rs";
        let byte_column = line.find('語').unwrap();
        assert_eq!(unicode_scalar_column(line, byte_column), Some(6));
        assert_eq!(unicode_scalar_column(line, 3), None);
    }

    #[test]
    fn latest_search_publishes_only_the_newest_completion() {
        let mut reducer = LatestSearch::<String, Vec<&str>, &'static str>::default();
        let query_a = reducer.begin("A".to_owned()).unwrap();
        let query_b = reducer.begin("B".to_owned()).unwrap();
        let mut publish_log = Vec::new();

        if reducer.complete(query_b, Ok(vec!["B-result"])) == CompletionDisposition::Published {
            publish_log.push("B");
        }
        if reducer.complete(query_a, Ok(vec!["A-result"])) == CompletionDisposition::Published {
            publish_log.push("A");
        }

        assert_eq!(publish_log, vec!["B"]);
        assert_eq!(
            reducer.state(),
            &LatestSearchState::Ready {
                generation: query_b,
                query: "B".to_owned(),
                result: vec!["B-result"],
            }
        );
    }

    #[test]
    fn cancellation_and_failure_have_explicit_states() {
        let mut reducer = LatestSearch::<&str, (), &str>::default();
        let cancelled = reducer.begin("cancelled").unwrap();
        reducer.cancel();
        assert_eq!(
            reducer.complete(cancelled, Ok(())),
            CompletionDisposition::DiscardedStale
        );
        assert_eq!(reducer.state(), &LatestSearchState::Idle);

        let failed = reducer.begin("failed").unwrap();
        assert_eq!(
            reducer.complete(failed, Err("EIO")),
            CompletionDisposition::Published
        );
        assert_eq!(
            reducer.state(),
            &LatestSearchState::Failed {
                generation: failed,
                query: "failed",
                error: "EIO",
            }
        );
    }
    fn run_control() -> LiteralSearchRunControl {
        LiteralSearchRunControl {
            user: LiteralSearchCancellation::new(),
            internal: LiteralSearchCancellation::new(),
        }
    }

    fn literal_query(query: &str) -> Arc<SearchQuery> {
        Arc::new(
            SearchQuery::text(
                query,
                false,
                true,
                false,
                Default::default(),
                Default::default(),
                false,
                None,
            )
            .expect("literal query"),
        )
    }

    #[test]
    fn open_alias_buffers_are_ordered_and_deduplicated_deterministically() {
        let (sender, receiver) = mpsc::sync_channel(1);
        gpui_platform::headless().run(move |cx| {
            let representative = cx.new(|cx| Buffer::local("representative", cx));
            let alias_a = cx.new(|cx| Buffer::local("alias-a", cx));
            let alias_z = cx.new(|cx| Buffer::local("alias-z", cx));
            let representative_id = representative.read(cx).remote_id();
            let alias_a_id = alias_a.read(cx).remote_id();
            let alias_z_id = alias_z.read(cx).remote_id();
            let mut buffers = vec![
                OpenLiteralSearchBuffer {
                    buffer: alias_z,
                    buffer_id: alias_z_id,
                    actual_path: Arc::from("z-alias.rs"),
                    actual_is_representative: false,
                },
                OpenLiteralSearchBuffer {
                    buffer: alias_a.clone(),
                    buffer_id: alias_a_id,
                    actual_path: Arc::from("a-alias.rs"),
                    actual_is_representative: false,
                },
                OpenLiteralSearchBuffer {
                    buffer: representative,
                    buffer_id: representative_id,
                    actual_path: Arc::from("src/real.rs"),
                    actual_is_representative: true,
                },
                OpenLiteralSearchBuffer {
                    buffer: alias_a,
                    buffer_id: alias_a_id,
                    actual_path: Arc::from("a-alias.rs"),
                    actual_is_representative: false,
                },
            ];
            order_open_literal_search_buffers(&mut buffers);
            let order = buffers
                .into_iter()
                .map(|buffer| {
                    (
                        buffer.actual_is_representative,
                        buffer.actual_path.to_string(),
                        buffer.buffer_id.to_string(),
                    )
                })
                .collect::<Vec<_>>();
            sender
                .send((
                    order,
                    representative_id.to_string(),
                    alias_a_id.to_string(),
                    alias_z_id.to_string(),
                ))
                .expect("send alias order");
            cx.spawn(async move |cx| {
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });

        let (order, representative_id, alias_a_id, alias_z_id) =
            receiver.recv().expect("receive alias order");
        assert_eq!(
            order,
            vec![
                (true, "src/real.rs".to_owned(), representative_id),
                (false, "a-alias.rs".to_owned(), alias_a_id),
                (false, "z-alias.rs".to_owned(), alias_z_id),
            ]
        );
    }

    #[test]
    fn source_budget_flags_only_the_first_excess_file_or_range() {
        let mut files = ProjectSearchSourceBudget::default();
        for _ in 0..PROJECT_SEARCH_SOURCE_FILE_LIMIT {
            assert_eq!(files.admit(1, false), (1, false));
        }
        assert_eq!(files.matched_files, PROJECT_SEARCH_SOURCE_FILE_LIMIT);
        assert_eq!(files.admit(1, false), (0, true));
        assert_eq!(files.ranges, PROJECT_SEARCH_SOURCE_FILE_LIMIT);

        let mut ranges = ProjectSearchSourceBudget::default();
        assert_eq!(
            ranges.admit(PROJECT_SEARCH_SOURCE_RANGE_LIMIT, false),
            (PROJECT_SEARCH_SOURCE_RANGE_LIMIT, false)
        );
        assert_eq!(ranges.admit(1, false), (0, true));
        assert_eq!(ranges.ranges, PROJECT_SEARCH_SOURCE_RANGE_LIMIT);

        let mut worker_overflow = ProjectSearchSourceBudget::default();
        assert_eq!(
            worker_overflow.admit(PROJECT_SEARCH_SOURCE_RANGE_LIMIT, true),
            (PROJECT_SEARCH_SOURCE_RANGE_LIMIT, true)
        );
    }

    #[test]
    fn ordered_worker_error_waits_for_its_ordinal_and_propagates() {
        let mut ordered = OrderedFileResults::default();
        assert!(
            ordered
                .insert(
                    1,
                    LiteralSearchWorkerResult::failure(1, anyhow::anyhow!("controlled EIO")),
                )
                .is_empty()
        );
        let ready = ordered.insert(0, LiteralSearchWorkerResult::empty(0));
        assert_eq!(ready.len(), 2);
        let mut candidates = OrderedSearchCandidates::default();
        let mut ready = ready.into_iter();
        assert!(matches!(
            candidates.push(ready.next().expect("ordinal zero")),
            OrderedSearchDisposition::Continue
        ));
        match candidates.push(ready.next().expect("ordinal one")) {
            OrderedSearchDisposition::Error(error) => {
                assert_eq!(error.to_string(), "controlled EIO");
            }
            _ => panic!("ordered worker error was not propagated"),
        }
    }

    #[test]
    fn ordered_file_results_retire_every_ordinal_deterministically() {
        for permutation in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let mut ordered = OrderedFileResults::default();
            let mut retired = Vec::new();
            for ordinal in permutation {
                retired.extend(ordered.insert(ordinal, ordinal));
            }
            assert_eq!(retired, vec![0, 1, 2], "permutation {permutation:?}");
        }
    }

    #[test]
    fn project_search_worker_count_is_cpu_bounded() {
        assert_eq!(project_search_worker_count(0), 1);
        assert_eq!(project_search_worker_count(1), 1);
        assert_eq!(project_search_worker_count(2), 2);
        assert_eq!(project_search_worker_count(4), 4);
        assert_eq!(project_search_worker_count(64), 4);
    }

    #[test]
    fn cancelled_reader_returns_non_retryable_read_line_error() {
        let control = run_control();
        control.user.cancel();
        let inner = std::io::Cursor::new(b"never read\n".to_vec());
        let mut reader = BufReader::new(CancellableReader::new(inner, control));
        let mut line = String::new();

        let error = reader
            .read_line(&mut line)
            .expect_err("cancelled read_line must terminate");
        assert_eq!(error.kind(), ErrorKind::Other);
        assert!(line.is_empty());
    }

    #[test]
    fn io_error_kind_classification_preserves_only_not_found_skip() {
        let not_found =
            anyhow::Error::new(io::Error::new(ErrorKind::NotFound, "gone")).context("wrapped open");
        let denied = anyhow::Error::new(io::Error::new(ErrorKind::PermissionDenied, "denied"))
            .context("wrapped open");
        let eio = anyhow::Error::new(io::Error::other("EIO")).context("wrapped open");

        assert!(error_has_io_kind(&not_found, ErrorKind::NotFound));
        assert!(!error_has_io_kind(&denied, ErrorKind::NotFound));
        assert!(!error_has_io_kind(&eio, ErrorKind::NotFound));
    }

    #[test]
    fn cancellation_wakes_blocked_worker_and_task_is_joined() {
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        gpui_platform::headless().run(move |cx| {
            let cancellation = LiteralSearchCancellation::new();
            let cancellation_probe = cancellation.clone();
            let control = LiteralSearchRunControl {
                user: cancellation,
                internal: LiteralSearchCancellation::new(),
            };
            let (input_sender, input_receiver) = async_channel::bounded::<u8>(1);
            let receiver_guard = input_receiver.clone();

            cx.spawn(async move |cx| {
                let worker = cx.background_spawn(async move {
                    recv_unless_stopped(&input_receiver, &control).await
                });
                cancellation_probe.cancel();
                let result = worker.await;
                result_sender
                    .send((result, input_sender.is_closed()))
                    .expect("send joined cancellation result");
                drop(receiver_guard);
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });

        let (result, input_was_closed) = result_receiver
            .recv()
            .expect("receive joined cancellation result");
        assert_eq!(result, None);
        assert!(
            !input_was_closed,
            "worker completed because its input closed instead of cancellation"
        );
    }

    #[test]
    fn snapshot_chunks_match_whole_search_for_every_split_phase_and_utf8_boundary() {
        let phase_text = "aaaaa".to_owned();
        let unicode_text = format!(
            "{}éneedle-tail",
            "x".repeat(PROJECT_SEARCH_SNAPSHOT_CHUNK_BYTES - 1)
        );
        let unicode_start = unicode_text.find("needle").expect("unicode match");
        let unicode_expected = vec![unicode_start..unicode_start + "needle".len()];
        let (result_sender, result_receiver) = mpsc::sync_channel(1);

        gpui_platform::headless().run(move |cx| {
            let phase_buffer = cx.new(|cx| Buffer::local(phase_text, cx));
            let phase_snapshot = phase_buffer.read(cx).snapshot();
            let unicode_buffer = cx.new(|cx| Buffer::local(unicode_text, cx));
            let unicode_snapshot = unicode_buffer.read(cx).snapshot();

            cx.spawn(async move |cx| {
                let whole_query = literal_query("aa");
                let whole = whole_query.search(&phase_snapshot, None).await;
                let mut split_results = Vec::new();
                for chunk_bytes in 1..=8 {
                    let snapshot_for_offsets = phase_snapshot.clone();
                    let split = cx
                        .background_spawn(search_snapshot_with_chunk_bytes(
                            whole_query.clone(),
                            phase_snapshot.clone(),
                            MatchPositionHint::default(),
                            run_control(),
                            chunk_bytes,
                        ))
                        .await;
                    let offsets = split
                        .ranges
                        .iter()
                        .map(|range| {
                            snapshot_for_offsets.summary_for_anchor(&range.start)
                                ..snapshot_for_offsets.summary_for_anchor(&range.end)
                        })
                        .collect::<Vec<_>>();
                    split_results.push((chunk_bytes, offsets, split.source_limit_reached));
                }

                let unicode_snapshot_for_offsets = unicode_snapshot.clone();
                let unicode = cx
                    .background_spawn(search_snapshot_in_chunks(
                        literal_query("needle"),
                        unicode_snapshot,
                        MatchPositionHint::default(),
                        run_control(),
                    ))
                    .await;
                let unicode_offsets = unicode
                    .ranges
                    .iter()
                    .map(|range| {
                        unicode_snapshot_for_offsets.summary_for_anchor(&range.start)
                            ..unicode_snapshot_for_offsets.summary_for_anchor(&range.end)
                    })
                    .collect::<Vec<_>>();
                result_sender
                    .send((
                        whole,
                        split_results,
                        unicode_offsets,
                        unicode.source_limit_reached,
                    ))
                    .expect("send chunk results");
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });

        let (whole, split_results, unicode_offsets, unicode_limit) =
            result_receiver.recv().expect("receive chunk results");
        for (chunk_bytes, offsets, limit) in split_results {
            assert_eq!(offsets, whole, "split phase for {chunk_bytes} bytes");
            assert!(!limit);
        }
        assert_eq!(unicode_offsets, unicode_expected);
        assert!(!unicode_limit);
    }

    #[test]
    fn snapshot_search_caps_each_worker_range_vector() {
        let text = "a".repeat(PROJECT_SEARCH_SOURCE_RANGE_LIMIT + 17);
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        gpui_platform::headless().run(move |cx| {
            let buffer = cx.new(|cx| Buffer::local(text, cx));
            let snapshot = buffer.read(cx).snapshot();
            cx.spawn(async move |cx| {
                let result = cx
                    .background_spawn(search_snapshot_in_chunks(
                        literal_query("a"),
                        snapshot,
                        MatchPositionHint::default(),
                        run_control(),
                    ))
                    .await;
                result_sender
                    .send((result.ranges.len(), result.source_limit_reached))
                    .expect("send range cap result");
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });

        assert_eq!(
            result_receiver.recv().expect("receive range cap result"),
            (PROJECT_SEARCH_SOURCE_RANGE_LIMIT, true)
        );
    }

    #[test]
    fn disk_prefilter_handles_binary_bom_and_exact_unicode() {
        let directory = tempfile::tempdir().expect("temporary search files");
        let binary = directory.path().join("binary.png");
        let bom = directory.path().join("bom.txt");
        let nfd = directory.path().join("nfd.txt");
        let missing = directory.path().join("missing.txt");
        std::fs::write(&binary, b"\x89PNG\r\n\x1a\ncaf\xc3\xa9").expect("write binary fixture");
        let mut bom_bytes = vec![0xEF, 0xBB, 0xBF];
        bom_bytes.extend_from_slice("café\n".as_bytes());
        std::fs::write(&bom, bom_bytes).expect("write BOM fixture");
        std::fs::write(&nfd, "cafe\u{301}\n").expect("write NFD fixture");

        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        gpui_platform::headless().run(move |cx| {
            let fs: Arc<dyn Fs> =
                Arc::new(zed_fs::RealFs::new(None, cx.background_executor().clone()));
            cx.spawn(async move |cx| {
                let query = literal_query("café");
                let result: Result<_> = async {
                    let binary =
                        detect_literal_candidate(query.clone(), fs.clone(), binary, run_control())
                            .await?;
                    let bom =
                        detect_literal_candidate(query.clone(), fs.clone(), bom, run_control())
                            .await?;
                    let nfd =
                        detect_literal_candidate(query.clone(), fs.clone(), nfd, run_control())
                            .await?;
                    let missing_is_error =
                        detect_literal_candidate(query, fs, missing, run_control())
                            .await
                            .is_err();
                    Ok((
                        binary.is_some(),
                        bom.is_some(),
                        nfd.is_some(),
                        missing_is_error,
                    ))
                }
                .await;
                result_sender
                    .send(result)
                    .expect("send disk prefilter result");
                let _ = cx.update(|cx| cx.quit());
            })
            .detach();
        });

        assert_eq!(
            result_receiver
                .recv()
                .expect("receive disk prefilter result")
                .expect("disk prefilter"),
            (false, true, false, false)
        );
    }
}
