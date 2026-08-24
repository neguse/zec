//! Repository-scoped discovery and search coordination.
//!
//! This module deliberately does not own file text, selections, undo state, or
//! dirty state. Zed's worktree supplies the file set and ignore decisions,
//! Zed's project search supplies buffers and anchor ranges, and `BufferStore`
//! remains the authority used by the caller to open a selected result. The
//! types here only provide the CLI-specific root identity, deterministic
//! presentation, symlink-alias coalescing, and latest-request reducer.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    fmt,
    hash::{Hash, Hasher},
    ops::Range,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
};

use anyhow::{Context as _, Result, bail, ensure};
use gpui::{App, AsyncApp, Entity};
use language::{Buffer, Point};
use project::{
    ProjectPath, Search, SearchResults, WorktreeId,
    buffer_store::BufferStore,
    search::{SearchQuery, SearchResult},
    worktree_store::WorktreeStore,
};
use zed_fs::Fs;

/// Alpha 1's fixed number of project-search rows shown to the user.
pub const PROJECT_SEARCH_DISPLAY_LIMIT: usize = 100;

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
            if (entry.ignored && !entry.always_included) || entry.external {
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

/// Start one case-sensitive, literal, non-ignored Zed project search.
///
/// Superseding callers should close its result stream through the cancellation
/// handle, then retain the running task until `collect` finishes its cooperative
/// unwind. A raced completion still requires [`LatestSearch`] invalidation.
pub fn start_literal_project_search(
    query: impl Into<String>,
    fs: Arc<dyn Fs>,
    buffer_store: Entity<BufferStore>,
    worktree_store: Entity<WorktreeStore>,
    cx: &mut App,
) -> Result<RunningLiteralSearch> {
    let query = query.into();
    ensure!(!query.is_empty(), "project search query cannot be empty");
    ensure!(
        !query.contains(['\r', '\n']),
        "project search query must fit on one prompt line"
    );
    let zed_query = SearchQuery::text(
        &query,
        false,
        true,
        false,
        Default::default(),
        Default::default(),
        false,
        None,
    )?;
    // Zed still enforces its range cap. `usize::MAX` avoids adding a second,
    // lower file-count limit before Alpha 1's fixed 100-row presentation cap.
    let results = Search::local(fs, buffer_store, worktree_store, usize::MAX, cx)
        .into_handle(zed_query, cx)
        .results(cx);
    let cancelled = Arc::new(AtomicBool::new(false));
    Ok(RunningLiteralSearch {
        query: query.into(),
        results,
        cancelled,
    })
}

#[must_use = "a running project search must be collected to completion"]
pub struct RunningLiteralSearch {
    query: Arc<str>,
    results: SearchResults<SearchResult>,
    cancelled: Arc<AtomicBool>,
}

/// Best-effort cooperative cancellation for a running Zed search stream.
/// Closing the receiver requests a stop but is not a completion acknowledgement.
#[derive(Clone)]
pub struct RunningLiteralSearchCancellation {
    cancelled: Arc<AtomicBool>,
    results: async_channel::Receiver<SearchResult>,
}

impl RunningLiteralSearchCancellation {
    pub fn cancel(&self) {
        self.cancelled.store(true, AtomicOrdering::Release);
        self.results.close();
    }

    #[cfg(test)]
    pub(crate) fn test_probe() -> Self {
        let (_sender, results) = async_channel::unbounded();
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            results,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(AtomicOrdering::Acquire)
    }
}

impl RunningLiteralSearch {
    pub fn cancellation_handle(&self) -> RunningLiteralSearchCancellation {
        RunningLiteralSearchCancellation {
            cancelled: self.cancelled.clone(),
            results: self.results.rx.clone(),
        }
    }

    /// Drain Zed's search stream and project anchor ranges into deterministic
    /// terminal rows. Each hit retains the Zed buffer and anchor range used to
    /// open/apply it; the preview is presentation data, not editable state.
    pub async fn collect(
        self,
        repository: &RepositoryIndex,
        cx: &mut AsyncApp,
    ) -> Result<ProjectSearchOutput> {
        let RunningLiteralSearch {
            query,
            results,
            cancelled,
        } = self;
        let SearchResults { task_handle, rx } = results;
        let mut source_limit_reached = false;
        let mut candidates = Vec::new();

        while let Ok(result) = rx.recv().await {
            if cancelled.load(AtomicOrdering::Acquire) {
                break;
            }
            match result {
                SearchResult::Buffer { buffer, ranges } => {
                    let buffer_for_hits = buffer.clone();
                    let projected = buffer.read_with(cx, |buffer, cx| {
                        project_buffer_matches(repository, buffer_for_hits, buffer, ranges, cx)
                    });
                    candidates.extend(projected);
                }
                SearchResult::LimitReached => source_limit_reached = true,
                SearchResult::WaitingForScan | SearchResult::Searching => {}
            }
        }

        // Receiver close is only a cooperative stop request. Awaiting Zed's
        // task acknowledges completion and avoids dropping its scoped worker
        // pool on a GPUI executor thread.
        task_handle.await;
        if cancelled.load(AtomicOrdering::Acquire) {
            bail!("project search cancelled");
        }

        // Project search can encounter both a symlink and its target. Prefer a
        // result whose actual ProjectPath is the representative path, then
        // discard identity+range duplicates.
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
        Ok(ProjectSearchOutput {
            query,
            matches,
            total_hits,
            source_limit_reached,
        })
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
    use std::collections::HashSet;

    fn root(requested: &str, canonical: &str) -> RepositoryRoot {
        RepositoryRoot::from_paths(requested.into(), canonical.into()).unwrap()
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
                    .with_canonical_path("/control/outside")
                    .external(true),
                RepositoryEntry::file("unmarked-outside", 4)
                    .with_canonical_path("/control/unmarked-outside"),
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
    fn symlink_aliases_share_one_identity_and_prefer_the_real_path() {
        let index = RepositoryIndex::from_entries(
            root("/repo", "/repo"),
            [
                RepositoryEntry::file("z-alias.rs", 20).with_canonical_path("/repo/src/real.rs"),
                RepositoryEntry::file("src/real.rs", 20),
                RepositoryEntry::file("a-alias.rs", 20).with_canonical_path("/repo/src/real.rs"),
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
                .file_for_canonical_path(Path::new("/repo/src/real.rs"))
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
}
