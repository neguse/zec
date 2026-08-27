//! Presentation-neutral project-search options, history, and replace preview.
//!
//! Zed's [`SearchQuery`] remains the search and replacement authority. This
//! module owns only terminal input validation and the all-or-nothing boundary
//! around a multi-buffer replacement. In particular, building a preview never
//! mutates a Buffer, and applying one first revalidates every participating
//! source before producing any replacement text.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ops::Range,
    sync::Arc,
};

use anyhow::{Context as _, Result, ensure};
use gpui::Entity;
use language::Buffer;
use project::search::SearchQuery;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use util::paths::{PathMatcher, PathStyle};

const MAX_QUERY_BYTES: usize = 64 * 1024;
const MAX_REPLACEMENT_BYTES: usize = 2 * 1024 * 1024;
const MAX_GLOB_COUNT: usize = 1_000;
const MAX_GLOB_BYTES: usize = 64 * 1024;
const MAX_HISTORY_ENTRIES: usize = 100;
const MAX_REPLACE_SOURCES: usize = 5_000;
const MAX_REPLACE_EDITS: usize = 10_000;
const MAX_REPLACE_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    #[default]
    Literal,
    Regex,
}

/// Exact user-visible project-search state. The open-buffer entities are not
/// stored here; callers supply the current Zed entities when compiling.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectSearchOptions {
    pub query: String,
    pub replacement: String,
    pub mode: SearchMode,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub include_ignored: bool,
    pub open_buffers_only: bool,
    pub match_full_paths: bool,
    pub include_globs: Vec<String>,
    pub exclude_globs: Vec<String>,
}

impl ProjectSearchOptions {
    /// Validate all terminal-controlled payloads, compile the regex and globs,
    /// and return Zed's own query object. Invalid input has no side effects.
    pub fn compile(
        &self,
        path_style: PathStyle,
        open_buffers: Vec<Entity<Buffer>>,
    ) -> Result<Arc<SearchQuery>> {
        self.validate()?;
        let include = PathMatcher::new(&self.include_globs, path_style)
            .context("invalid project-search include glob")?;
        let exclude = PathMatcher::new(&self.exclude_globs, path_style)
            .context("invalid project-search exclude glob")?;
        let buffers = self.open_buffers_only.then_some(open_buffers);
        let query = match self.mode {
            SearchMode::Literal => SearchQuery::text(
                &self.query,
                self.whole_word,
                self.case_sensitive,
                self.include_ignored,
                include,
                exclude,
                self.match_full_paths,
                buffers,
            ),
            SearchMode::Regex => SearchQuery::regex(
                &self.query,
                self.whole_word,
                self.case_sensitive,
                self.include_ignored,
                false,
                include,
                exclude,
                self.match_full_paths,
                buffers,
            ),
        }
        .context("invalid project-search query")?
        .with_replacement(self.replacement.clone());
        Ok(Arc::new(query))
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.query.is_empty(),
            "project search query cannot be empty"
        );
        ensure!(
            self.query.len() <= MAX_QUERY_BYTES,
            "project search query exceeds {MAX_QUERY_BYTES} bytes"
        );
        ensure!(
            !self.query.contains(['\r', '\n', '\0']),
            "project search query must fit on one prompt line"
        );
        ensure!(
            self.replacement.len() <= MAX_REPLACEMENT_BYTES,
            "project search replacement exceeds {MAX_REPLACEMENT_BYTES} bytes"
        );
        ensure!(
            !self.replacement.contains('\0'),
            "project search replacement contains NUL"
        );
        validate_globs("include", &self.include_globs)?;
        validate_globs("exclude", &self.exclude_globs)?;
        Ok(())
    }
}

fn validate_globs(kind: &str, globs: &[String]) -> Result<()> {
    ensure!(
        globs.len() <= MAX_GLOB_COUNT,
        "project search {kind} list exceeds {MAX_GLOB_COUNT} globs"
    );
    let total = globs.iter().try_fold(0usize, |total, glob| {
        ensure!(!glob.is_empty(), "project search {kind} glob is empty");
        ensure!(
            !glob.contains(['\r', '\n', '\0']),
            "project search {kind} glob contains a control character"
        );
        total
            .checked_add(glob.len())
            .context("project search glob byte count overflow")
    })?;
    ensure!(
        total <= MAX_GLOB_BYTES,
        "project search {kind} list exceeds {MAX_GLOB_BYTES} bytes"
    );
    Ok(())
}

/// Bounded newest-first history shared by search options and replacement text.
#[derive(Clone, Debug)]
pub struct ProjectSearchHistory {
    limit: usize,
    entries: VecDeque<ProjectSearchOptions>,
}

impl Default for ProjectSearchHistory {
    fn default() -> Self {
        Self::new(MAX_HISTORY_ENTRIES)
    }
}

impl ProjectSearchHistory {
    pub fn new(limit: usize) -> Self {
        Self {
            limit: limit.min(MAX_HISTORY_ENTRIES),
            entries: VecDeque::new(),
        }
    }

    pub fn record(&mut self, options: ProjectSearchOptions) -> Result<()> {
        options.validate()?;
        if self.limit == 0 {
            return Ok(());
        }
        if let Some(index) = self.entries.iter().position(|entry| entry == &options) {
            self.entries.remove(index);
        }
        self.entries.push_front(options);
        self.entries.truncate(self.limit);
        Ok(())
    }

    pub fn entries(&self) -> impl ExactSizeIterator<Item = &ProjectSearchOptions> {
        self.entries.iter()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ContentFingerprint {
    pub bytes: u64,
    pub sha256: [u8; 32],
}

impl ContentFingerprint {
    pub fn for_bytes(bytes: &[u8]) -> Self {
        Self {
            bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            sha256: Sha256::digest(bytes).into(),
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ReplaceSourceIdentity {
    pub buffer_id: u64,
    pub path: Arc<str>,
}

#[derive(Clone, Debug)]
pub struct ReplaceSourceSnapshot {
    pub identity: ReplaceSourceIdentity,
    pub text: Arc<str>,
    /// Fingerprint of the disk bytes observed with this Buffer snapshot. `None`
    /// is reserved for scratch and otherwise non-file-backed sources.
    pub disk: Option<ContentFingerprint>,
}

impl ReplaceSourceSnapshot {
    pub fn new(
        buffer_id: u64,
        path: impl Into<Arc<str>>,
        text: impl Into<Arc<str>>,
        disk: Option<ContentFingerprint>,
    ) -> Self {
        Self {
            identity: ReplaceSourceIdentity {
                buffer_id,
                path: path.into(),
            },
            text: text.into(),
            disk,
        }
    }

    pub fn buffer_fingerprint(&self) -> ContentFingerprint {
        ContentFingerprint::for_bytes(self.text.as_bytes())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplaceMatch {
    pub source: ReplaceSourceIdentity,
    pub range: Range<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplaceScope {
    One(ReplaceMatch),
    File(ReplaceSourceIdentity),
    All,
}

impl ReplaceScope {
    pub fn select(&self, matches: &[ReplaceMatch]) -> Vec<ReplaceMatch> {
        match self {
            Self::One(selected) => matches
                .iter()
                .filter(|candidate| *candidate == selected)
                .cloned()
                .collect(),
            Self::File(source) => matches
                .iter()
                .filter(|candidate| &candidate.source == source)
                .cloned()
                .collect(),
            Self::All => matches.to_vec(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplaceEditPreview {
    pub range: Range<usize>,
    pub original: Arc<str>,
    pub replacement: Arc<str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplaceFilePreview {
    pub source: ReplaceSourceIdentity,
    pub before: ContentFingerprint,
    pub disk: Option<ContentFingerprint>,
    pub after: ContentFingerprint,
    pub edits: Vec<ReplaceEditPreview>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplacePreview {
    pub search_generation: u64,
    pub files: Vec<ReplaceFilePreview>,
    pub total_edits: usize,
    pub total_replacement_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplacementOutput {
    pub source: ReplaceSourceIdentity,
    pub text: Arc<str>,
    pub fingerprint: ContentFingerprint,
}

impl ReplacePreview {
    /// Build an immutable diff preview from exact Zed search ranges. Every
    /// selected range must still be a real hit in the supplied source.
    pub fn build(
        search_generation: u64,
        query: &SearchQuery,
        sources: &[ReplaceSourceSnapshot],
        matches: Vec<ReplaceMatch>,
    ) -> Result<Self> {
        ensure!(search_generation > 0, "search generation must be non-zero");
        ensure!(
            sources.len() <= MAX_REPLACE_SOURCES,
            "replace preview exceeds {MAX_REPLACE_SOURCES} sources"
        );
        ensure!(
            matches.len() <= MAX_REPLACE_EDITS,
            "replace preview exceeds {MAX_REPLACE_EDITS} edits"
        );
        ensure!(
            query.replacement().is_some(),
            "replace preview requires replacement text"
        );

        let mut source_by_identity = BTreeMap::new();
        let mut buffer_ids = BTreeSet::new();
        for source in sources {
            ensure!(
                source_by_identity
                    .insert(source.identity.clone(), source)
                    .is_none(),
                "replace preview contains duplicate source {}",
                source.identity.path
            );
            ensure!(
                buffer_ids.insert(source.identity.buffer_id),
                "replace preview contains aliased Buffer id {}",
                source.identity.buffer_id
            );
        }

        let mut matches = matches;
        matches.sort_by(|left, right| {
            left.source
                .cmp(&right.source)
                .then_with(|| left.range.start.cmp(&right.range.start))
                .then_with(|| left.range.end.cmp(&right.range.end))
        });
        matches.dedup();

        let mut matches_by_source: BTreeMap<ReplaceSourceIdentity, Vec<Range<usize>>> =
            BTreeMap::new();
        for selected in matches {
            let source = source_by_identity.get(&selected.source).with_context(|| {
                format!(
                    "replace match references unknown source {}",
                    selected.source.path
                )
            })?;
            validate_range(&source.text, &selected.range)?;
            matches_by_source
                .entry(selected.source)
                .or_default()
                .push(selected.range);
        }

        let mut files = Vec::with_capacity(matches_by_source.len());
        let mut total_edits = 0usize;
        let mut total_replacement_bytes = 0usize;
        let mut total_output_bytes = 0usize;
        for (identity, ranges) in matches_by_source {
            let source = source_by_identity
                .get(&identity)
                .expect("grouped replace source disappeared");
            validate_non_overlapping(&ranges)?;
            let actual_matches = query
                .search_str(&source.text)
                .into_iter()
                .map(|range| (range.start, range.end))
                .collect::<BTreeSet<_>>();
            let mut edits = Vec::with_capacity(ranges.len());
            for range in ranges {
                ensure!(
                    actual_matches.contains(&(range.start, range.end)),
                    "replace range {}..{} is no longer a query match in {}",
                    range.start,
                    range.end,
                    identity.path
                );
                let original: Arc<str> = Arc::from(
                    source
                        .text
                        .get(range.clone())
                        .context("replace range is not a UTF-8 boundary")?,
                );
                let replacement = replacement_for_range(query, &source.text, range.clone())?
                    .context("query has no replacement")?;
                total_replacement_bytes = total_replacement_bytes
                    .checked_add(replacement.len())
                    .context("replace preview byte count overflow")?;
                ensure!(
                    total_replacement_bytes <= MAX_REPLACEMENT_BYTES,
                    "replace preview exceeds {MAX_REPLACEMENT_BYTES} replacement bytes"
                );
                edits.push(ReplaceEditPreview {
                    range,
                    original,
                    replacement: Arc::from(replacement),
                });
            }
            total_edits = total_edits
                .checked_add(edits.len())
                .context("replace preview edit count overflow")?;
            let after_text = apply_edits(&source.text, &edits)?;
            total_output_bytes = total_output_bytes
                .checked_add(after_text.len())
                .context("replace preview output byte count overflow")?;
            ensure!(
                total_output_bytes <= MAX_REPLACE_OUTPUT_BYTES,
                "replace preview exceeds {MAX_REPLACE_OUTPUT_BYTES} output bytes"
            );
            files.push(ReplaceFilePreview {
                source: identity,
                before: source.buffer_fingerprint(),
                disk: source.disk,
                after: ContentFingerprint::for_bytes(after_text.as_bytes()),
                edits,
            });
        }

        Ok(Self {
            search_generation,
            files,
            total_edits,
            total_replacement_bytes,
        })
    }

    /// Revalidate every Buffer and disk fingerprint before producing any
    /// output. A single stale or missing source aborts the whole batch.
    pub fn apply_checked(
        &self,
        current_generation: u64,
        current: &[ReplaceSourceSnapshot],
    ) -> Result<Vec<ReplacementOutput>> {
        ensure!(
            current_generation == self.search_generation,
            "replace preview belongs to stale search generation {}",
            self.search_generation
        );
        let current_by_identity = current
            .iter()
            .map(|source| (source.identity.clone(), source))
            .collect::<BTreeMap<_, _>>();
        ensure!(
            current_by_identity.len() == current.len(),
            "current replace sources contain duplicate identities"
        );

        // Pass one performs only validation. Do not construct even a partial
        // replacement output until every source has passed.
        for file in &self.files {
            let source = current_by_identity
                .get(&file.source)
                .with_context(|| format!("replace source disappeared: {}", file.source.path))?;
            ensure!(
                source.buffer_fingerprint() == file.before,
                "replace source changed after preview: {}",
                file.source.path
            );
            ensure!(
                source.disk == file.disk,
                "replace source disk fingerprint changed after preview: {}",
                file.source.path
            );
            for edit in &file.edits {
                validate_range(&source.text, &edit.range)?;
                ensure!(
                    source.text.get(edit.range.clone()) == Some(edit.original.as_ref()),
                    "replace source range changed after preview: {}",
                    file.source.path
                );
            }
        }

        let mut outputs = Vec::with_capacity(self.files.len());
        for file in &self.files {
            let source = current_by_identity
                .get(&file.source)
                .expect("validated replace source disappeared");
            let text: Arc<str> = Arc::from(apply_edits(&source.text, &file.edits)?);
            let fingerprint = ContentFingerprint::for_bytes(text.as_bytes());
            ensure!(
                fingerprint == file.after,
                "replace preview output fingerprint mismatch for {}",
                file.source.path
            );
            outputs.push(ReplacementOutput {
                source: file.source.clone(),
                text,
                fingerprint,
            });
        }
        Ok(outputs)
    }
}

fn validate_range(text: &str, range: &Range<usize>) -> Result<()> {
    ensure!(range.start <= range.end, "replace range is reversed");
    ensure!(range.end <= text.len(), "replace range exceeds source text");
    ensure!(
        text.is_char_boundary(range.start) && text.is_char_boundary(range.end),
        "replace range splits a UTF-8 codepoint"
    );
    Ok(())
}

fn validate_non_overlapping(ranges: &[Range<usize>]) -> Result<()> {
    for pair in ranges.windows(2) {
        ensure!(
            pair[0].end <= pair[1].start,
            "replace ranges overlap at {}..{} and {}..{}",
            pair[0].start,
            pair[0].end,
            pair[1].start,
            pair[1].end
        );
    }
    Ok(())
}

fn replacement_for_range(
    query: &SearchQuery,
    text: &str,
    range: Range<usize>,
) -> Result<Option<String>> {
    if !query.replacement_requires_context() {
        return Ok(query.replacement().map(ToOwned::to_owned));
    }

    let matched = text
        .get(range.clone())
        .context("replace range is not a UTF-8 boundary")?;
    let (context, relative) = if matched.contains('\n') {
        (matched, 0..matched.len())
    } else {
        let line_start = text[..range.start].rfind('\n').map_or(0, |index| index + 1);
        let line_end = text[range.end..]
            .find('\n')
            .map_or(text.len(), |index| range.end + index);
        (
            &text[line_start..line_end],
            (range.start - line_start)..(range.end - line_start),
        )
    };
    Ok(query
        .replacement_for(context, relative)
        .map(|replacement| replacement.into_owned()))
}

fn apply_edits(text: &str, edits: &[ReplaceEditPreview]) -> Result<String> {
    let mut result = text.to_owned();
    for edit in edits.iter().rev() {
        validate_range(&result, &edit.range)?;
        result.replace_range(edit.range.clone(), &edit.replacement);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use util::rel_path::RelPath;

    fn options(query: &str, replacement: &str) -> ProjectSearchOptions {
        ProjectSearchOptions {
            query: query.to_owned(),
            replacement: replacement.to_owned(),
            case_sensitive: true,
            ..ProjectSearchOptions::default()
        }
    }

    fn compile(options: &ProjectSearchOptions) -> Arc<SearchQuery> {
        options
            .compile(PathStyle::Unix, Vec::new())
            .expect("compile query")
    }

    fn source(id: u64, path: &str, text: &str) -> ReplaceSourceSnapshot {
        ReplaceSourceSnapshot::new(id, Arc::<str>::from(path), Arc::<str>::from(text), None)
    }

    fn selected(source: &ReplaceSourceSnapshot, range: Range<usize>) -> ReplaceMatch {
        ReplaceMatch {
            source: source.identity.clone(),
            range,
        }
    }

    fn rel(path: &str) -> std::borrow::Cow<'_, RelPath> {
        RelPath::new(path.as_ref(), PathStyle::Unix).unwrap()
    }

    #[test]
    fn zed_query_owns_regex_case_word_and_path_semantics() {
        let configured = ProjectSearchOptions {
            query: "widget".into(),
            replacement: "component".into(),
            mode: SearchMode::Regex,
            case_sensitive: false,
            whole_word: true,
            include_globs: vec!["src/**/*.rs".into()],
            exclude_globs: vec!["src/generated/**".into()],
            ..ProjectSearchOptions::default()
        };
        let query = compile(&configured);
        assert!(query.is_regex());
        assert!(query.whole_word());
        assert!(!query.case_sensitive());
        assert_eq!(query.search_str("Widget widgets widget").len(), 2);
        assert!(query.match_path(&rel("src/ui/main.rs")));
        assert!(!query.match_path(&rel("src/generated/main.rs")));
        assert!(!query.match_path(&rel("tests/main.rs")));
    }

    #[test]
    fn invalid_regex_and_glob_fail_before_a_query_exists() {
        let mut invalid_regex = options("(", "x");
        invalid_regex.mode = SearchMode::Regex;
        assert!(
            invalid_regex
                .compile(PathStyle::Unix, Vec::new())
                .unwrap_err()
                .to_string()
                .contains("invalid project-search query")
        );

        let mut invalid_glob = options("x", "y");
        invalid_glob.include_globs = vec!["[".into()];
        assert!(
            invalid_glob
                .compile(PathStyle::Unix, Vec::new())
                .unwrap_err()
                .to_string()
                .contains("include glob")
        );
    }

    #[test]
    fn history_is_bounded_newest_first_and_deduplicated() {
        let mut history = ProjectSearchHistory::new(2);
        let first = options("one", "1");
        let second = options("two", "2");
        history.record(first.clone()).unwrap();
        history.record(second.clone()).unwrap();
        history.record(first.clone()).unwrap();
        assert_eq!(history.entries().collect::<Vec<_>>(), vec![&first, &second]);
        history.record(options("three", "3")).unwrap();
        assert_eq!(
            history
                .entries()
                .map(|entry| entry.query.as_str())
                .collect::<Vec<_>>(),
            vec!["three", "one"]
        );
    }

    #[test]
    fn capture_replacement_preview_is_deterministic_and_checked() {
        let mut configured = options(r"([a-z]+)=([0-9]+)", "$2:$1");
        configured.mode = SearchMode::Regex;
        let query = compile(&configured);
        let source = source(7, "src/data.txt", "alpha=12 beta=7\n");
        let matches = query
            .search_str(&source.text)
            .into_iter()
            .map(|range| selected(&source, range))
            .collect();
        let preview = ReplacePreview::build(4, &query, &[source.clone()], matches).unwrap();
        assert_eq!(preview.total_edits, 2);
        let output = preview.apply_checked(4, &[source]).unwrap();
        assert_eq!(&*output[0].text, "12:alpha 7:beta\n");
        assert_eq!(output[0].fingerprint, preview.files[0].after);
    }

    #[test]
    fn stale_buffer_or_disk_aborts_the_entire_batch() {
        let query = compile(&options("old", "new"));
        let disk_a = ContentFingerprint::for_bytes(b"old a");
        let disk_b = ContentFingerprint::for_bytes(b"old b");
        let mut first = source(1, "a.txt", "old a");
        first.disk = Some(disk_a);
        let mut second = source(2, "b.txt", "old b");
        second.disk = Some(disk_b);
        let preview = ReplacePreview::build(
            9,
            &query,
            &[first.clone(), second.clone()],
            vec![selected(&first, 0..3), selected(&second, 0..3)],
        )
        .unwrap();

        let changed_second = source(2, "b.txt", "old changed");
        let error = preview
            .apply_checked(9, &[first.clone(), changed_second])
            .unwrap_err();
        assert!(error.to_string().contains("b.txt"));
        assert!(preview.apply_checked(8, &[first, second]).is_err());
    }

    #[test]
    fn disk_fingerprint_change_aborts_even_when_buffer_text_matches() {
        let query = compile(&options("old", "new"));
        let mut before = source(1, "a.txt", "old");
        before.disk = Some(ContentFingerprint::for_bytes(b"old"));
        let preview =
            ReplacePreview::build(1, &query, &[before.clone()], vec![selected(&before, 0..3)])
                .unwrap();
        let mut current = before;
        current.disk = Some(ContentFingerprint::for_bytes(b"different disk bytes"));
        assert!(
            preview
                .apply_checked(1, &[current])
                .unwrap_err()
                .to_string()
                .contains("disk fingerprint")
        );
    }

    #[test]
    fn overlap_unknown_source_and_non_match_are_rejected() {
        let query = compile(&options("aaaa", "x"));
        let primary = source(1, "a.txt", "aaaaa");
        assert!(
            ReplacePreview::build(
                1,
                &query,
                &[primary.clone()],
                vec![selected(&primary, 0..4), selected(&primary, 1..5)]
            )
            .unwrap_err()
            .to_string()
            .contains("overlap")
        );
        assert!(
            ReplacePreview::build(
                1,
                &query,
                &[primary.clone()],
                vec![selected(&primary, 0..3)]
            )
            .is_err()
        );
        let unknown = source(2, "missing.txt", "aaaa");
        assert!(
            ReplacePreview::build(1, &query, &[primary], vec![selected(&unknown, 0..4)]).is_err()
        );
    }

    #[test]
    fn multiline_capture_replacement_uses_the_exact_match_as_context() {
        let mut configured = options("(?s)(left)\\n(right)", "$2-$1");
        configured.mode = SearchMode::Regex;
        let query = compile(&configured);
        let source = source(1, "a.txt", "before\nleft\nright\nafter\n");
        let range = query.search_str(&source.text).pop().unwrap();
        let preview =
            ReplacePreview::build(1, &query, &[source.clone()], vec![selected(&source, range)])
                .unwrap();
        assert_eq!(
            &*preview.apply_checked(1, &[source]).unwrap()[0].text,
            "before\nright-left\nafter\n"
        );
    }

    #[test]
    fn scopes_select_one_file_or_all_without_reordering() {
        let first = source(1, "a", "x x");
        let second = source(2, "b", "x");
        let matches = vec![
            selected(&first, 0..1),
            selected(&first, 2..3),
            selected(&second, 0..1),
        ];
        assert_eq!(
            ReplaceScope::One(matches[1].clone()).select(&matches),
            vec![matches[1].clone()]
        );
        assert_eq!(
            ReplaceScope::File(first.identity).select(&matches),
            matches[..2]
        );
        assert_eq!(ReplaceScope::All.select(&matches), matches);
    }
}
