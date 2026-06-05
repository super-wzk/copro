use crate::tools::utils::{read_file_bytes, resolve_path};
use crate::tools::vfs_walk::{
    compare_modified_desc_then_path, directory_entries, display_path, gitignore_is_ignored,
    is_under_vcs_dir, is_vcs_dir, load_ancestor_gitignores, load_gitignore_in_dir,
};
use coox_harness::tools::{Tool, ToolContext, ToolUpdatePayload};
use copro_agent::{CancellationToken, ToolExecutionPolicy};
use copro_api::async_trait;
use grep::regex::{RegexMatcher, RegexMatcherBuilder};
use grep::searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::gitignore::Gitignore;
use ignore::overrides::{Override, OverrideBuilder};
use ignore::types::{Types, TypesBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;
use std::time::SystemTime;
use vfs::VfsFileType;
use vfs::VfsMetadata;
use vfs::async_vfs::AsyncVfsPath;

pub const GREP_TOOL_NAME: &str = "grep";

const GREP_TOOL_DESCRIPTION: &str = concat!(
    "Search workspace files with ripgrep regex. Supports glob/type filters, ",
    "content/files_with_matches/count output modes, context lines, and multiline ",
    "matching. Respects .gitignore rules and always skips VCS directories. Use bash ",
    "with rg options for explicit ignored-file scans."
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepToolConfig {
    pub default_head_limit: usize,
}

impl Default for GrepToolConfig {
    fn default() -> Self {
        Self {
            default_head_limit: 250,
        }
    }
}

#[derive(Clone)]
pub struct GrepTool {
    root: AsyncVfsPath,
    config: GrepToolConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepMatchFound {
    pub path: String,
    pub line_number: Option<u64>,
    pub byte_offset: u64,
    pub line_count: usize,
}

impl ToolUpdatePayload for GrepMatchFound {
    const KIND: &'static str = "grep.match_found";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepProgress {
    pub searched_files: usize,
    pub matched_files: usize,
    pub current_path: Option<String>,
}

impl ToolUpdatePayload for GrepProgress {
    const KIND: &'static str = "grep.progress";
}

impl GrepTool {
    pub fn new(root: AsyncVfsPath) -> Self {
        Self::with_config(root, GrepToolConfig::default())
    }

    pub fn with_config(root: AsyncVfsPath, config: GrepToolConfig) -> Self {
        Self { root, config }
    }

    pub fn root(&self) -> &AsyncVfsPath {
        &self.root
    }

    pub fn config(&self) -> &GrepToolConfig {
        &self.config
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GrepOutputMode {
    /// Show matching line content.
    Content,
    /// Show only file paths that contain at least one match.
    #[default]
    FilesWithMatches,
    /// Show the number of matching lines per file.
    Count,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct GrepInput {
    /// Regular expression pattern to search for.
    pub pattern: String,
    /// File or directory to search. Defaults to the workspace root/current directory.
    #[serde(default)]
    pub path: Option<String>,
    /// File glob filter, e.g. "*.ts".
    #[serde(default)]
    pub glob: Option<String>,
    /// Output mode: content, files_with_matches, or count. Defaults to files_with_matches.
    #[serde(default)]
    pub output_mode: GrepOutputMode,
    /// Show N lines before each match.
    #[serde(default, rename = "-B")]
    pub before: Option<usize>,
    /// Show N lines after each match.
    #[serde(default, rename = "-A")]
    pub after: Option<usize>,
    /// Show N lines before and after each match.
    #[serde(default, rename = "-C")]
    pub context_flag: Option<usize>,
    /// Same as -C: show N lines before and after each match.
    #[serde(default)]
    pub context: Option<usize>,
    /// Show line numbers. Defaults to true.
    #[serde(default, rename = "-n")]
    pub line_numbers: Option<bool>,
    /// Case-insensitive search.
    #[serde(default, rename = "-i")]
    pub case_insensitive: bool,
    /// File type filter such as "js" or "py".
    #[serde(default, rename = "type")]
    pub file_type: Option<String>,
    /// Limit output lines. Defaults to 250. Use 0 for unlimited.
    #[serde(default)]
    pub head_limit: Option<usize>,
    /// Skip the first N output lines.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Enable multiline matching like `rg -U --multiline-dotall`.
    #[serde(default)]
    pub multiline: bool,
}

#[async_trait]
impl Tool for GrepTool {
    type Input = GrepInput;
    type Output = String;

    fn name(&self) -> &str {
        GREP_TOOL_NAME
    }

    fn description(&self) -> &str {
        GREP_TOOL_DESCRIPTION
    }

    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::Parallel
    }

    async fn call(&self, input: Self::Input, context: ToolContext) -> Result<Self::Output, String> {
        let cancel = context.cancellation().clone();
        if cancel.is_cancelled() {
            return Err("grep cancelled".to_string());
        }

        let matcher = build_matcher(&input)?;
        let filter = GrepFilter::new(&input)?;
        let search_path = input.path.as_deref().unwrap_or("");
        let search_options = SearchOptions::from_input(&input);
        let mut output = OutputCollector::new(
            input.offset.unwrap_or(0),
            input.head_limit.unwrap_or(self.config.default_head_limit),
        );

        let plan = SearchPlan {
            root: &self.root,
            context: &context,
            matcher: &matcher,
            filter: &filter,
            options: &search_options,
            mode: input.output_mode,
            cancel,
        };

        search_vfs(&self.root, search_path, &plan, &mut output).await?;

        Ok(output.finish())
    }
}

fn build_matcher(input: &GrepInput) -> Result<RegexMatcher, String> {
    let mut builder = RegexMatcherBuilder::new();
    builder.case_insensitive(input.case_insensitive);
    if input.multiline {
        builder.dot_matches_new_line(true);
    }
    builder
        .build(&input.pattern)
        .map_err(|error| format!("invalid regex pattern: {error}"))
}

fn build_searcher(options: &SearchOptions) -> Searcher {
    let mut builder = SearcherBuilder::new();
    builder
        .line_number(options.line_numbers)
        .before_context(options.before_context)
        .after_context(options.after_context)
        .multi_line(options.multiline)
        .binary_detection(BinaryDetection::quit(b'\x00'));
    builder.build()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SearchOptions {
    before_context: usize,
    after_context: usize,
    line_numbers: bool,
    multiline: bool,
}

impl SearchOptions {
    fn from_input(input: &GrepInput) -> Self {
        let context = input.context_flag.or(input.context).unwrap_or(0);
        Self {
            before_context: input.before.unwrap_or(context),
            after_context: input.after.unwrap_or(context),
            line_numbers: input.line_numbers.unwrap_or(true),
            multiline: input.multiline,
        }
    }
}

struct GrepFilter {
    glob: Option<Override>,
    file_type: Option<Types>,
}

impl GrepFilter {
    fn new(input: &GrepInput) -> Result<Self, String> {
        let glob = match &input.glob {
            Some(glob) => {
                let mut builder = OverrideBuilder::new("");
                builder
                    .add(glob)
                    .map_err(|error| format!("invalid glob `{glob}`: {error}"))?;
                Some(
                    builder
                        .build()
                        .map_err(|error| format!("invalid glob `{glob}`: {error}"))?,
                )
            }
            None => None,
        };

        let file_type = match &input.file_type {
            Some(file_type) => {
                let mut builder = TypesBuilder::new();
                builder.add_defaults();
                builder.select(file_type);
                Some(
                    builder
                        .build()
                        .map_err(|error| format!("invalid file type `{file_type}`: {error}"))?,
                )
            }
            None => None,
        };

        Ok(Self { glob, file_type })
    }

    fn matches(&self, root: &AsyncVfsPath, path: &AsyncVfsPath) -> bool {
        let display = display_path(root, path);
        let path = Path::new(&display);

        if let Some(glob) = &self.glob
            && !glob.matched(path, false).is_whitelist()
        {
            return false;
        }

        if let Some(file_type) = &self.file_type
            && !file_type.matched(path, false).is_whitelist()
        {
            return false;
        }

        true
    }
}

struct OutputCollector {
    offset: usize,
    limit: usize,
    seen: usize,
    lines: Vec<String>,
    truncated: bool,
    matched_files: Vec<MatchedFileSort>,
}

struct MatchedFileSort {
    modified: Option<SystemTime>,
}

impl OutputCollector {
    fn new(offset: usize, limit: usize) -> Self {
        Self {
            offset,
            limit,
            seen: 0,
            lines: Vec::new(),
            truncated: false,
            matched_files: Vec::new(),
        }
    }

    /// Push one logical output line. Returns false once the caller can stop searching.
    fn push(&mut self, line: String) -> bool {
        let index = self.seen;
        self.seen += 1;

        if index < self.offset {
            return true;
        }

        if self.limit == 0 || self.lines.len() < self.limit {
            self.lines.push(line);
            true
        } else {
            self.truncated = true;
            false
        }
    }

    fn record_matched_file(&mut self, modified: Option<SystemTime>) {
        self.matched_files.push(MatchedFileSort { modified });
    }

    fn should_stop(&self) -> bool {
        self.truncated
    }

    fn finish(self) -> String {
        if self.seen == 0 {
            return "No matches".to_string();
        }

        if self.lines.is_empty() {
            return format!(
                "No output: offset {} is past end ({} line(s))",
                self.offset, self.seen
            );
        }

        let sort_note = sort_note_for_matched_files(&self.matched_files);
        let mut output = self.lines.join("\n");
        if self.truncated {
            output.push_str(&format!(
                "\n[truncated: reached head_limit; continue with offset={}]",
                self.offset + self.lines.len()
            ));
        }
        if let Some(sort_note) = sort_note {
            output.push('\n');
            output.push_str(sort_note);
        }
        output
    }
}

fn sort_note_for_matched_files(files: &[MatchedFileSort]) -> Option<&'static str> {
    if files.len() <= 1 {
        return None;
    }

    let missing_modified_count = files.iter().filter(|file| file.modified.is_none()).count();
    match missing_modified_count {
        0 => None,
        count if count == files.len() => {
            Some("[sort: path order; modification time unavailable from VFS for matched files]")
        }
        _ => Some(
            "[sort: modification time descending; matched files without modification time sorted by path]",
        ),
    }
}

struct SearchPlan<'a> {
    root: &'a AsyncVfsPath,
    context: &'a ToolContext,
    matcher: &'a RegexMatcher,
    filter: &'a GrepFilter,
    options: &'a SearchOptions,
    mode: GrepOutputMode,
    cancel: CancellationToken,
}

async fn search_vfs(
    root: &AsyncVfsPath,
    input_path: &str,
    plan: &SearchPlan<'_>,
    output: &mut OutputCollector,
) -> Result<(), String> {
    let start = if input_path.is_empty() {
        root.clone()
    } else {
        resolve_path(root, input_path)?
    };

    if is_under_vcs_dir(&start) {
        return Ok(());
    }

    let metadata = start.metadata().await.map_err(|error| error.to_string())?;
    let is_dir = metadata.file_type == VfsFileType::Directory;
    let inherited_gitignores = load_ancestor_gitignores(root, &start).await?;

    if gitignore_is_ignored(&inherited_gitignores, &start, is_dir) {
        return Ok(());
    }

    let mut candidates = Vec::new();
    match metadata.file_type {
        VfsFileType::File => {
            if plan.filter.matches(plan.root, &start) {
                candidates.push(CandidateFile::new(plan.root, start, &metadata));
            }
        }
        VfsFileType::Directory => {
            collect_candidate_files(start, inherited_gitignores, plan, &mut candidates).await?;
        }
    }

    sort_candidate_files_by_modified_desc(&mut candidates);
    let mut searched_files = 0;
    let mut matched_files = 0;
    for candidate in candidates {
        if plan.cancel.is_cancelled() {
            return Err("grep cancelled".to_string());
        }
        if output.should_stop() {
            break;
        }
        searched_files += 1;
        emit_grep_progress(
            plan.context,
            searched_files,
            matched_files,
            Some(&candidate),
        )
        .await?;
        if search_one_file(&candidate, plan, output).await? {
            matched_files += 1;
            output.record_matched_file(candidate.modified);
        }
    }

    emit_grep_progress(plan.context, searched_files, matched_files, None).await?;

    Ok(())
}

struct CandidateFile {
    path: AsyncVfsPath,
    display_path: String,
    byte_len: u64,
    modified: Option<SystemTime>,
}

impl CandidateFile {
    fn new(root: &AsyncVfsPath, path: AsyncVfsPath, metadata: &VfsMetadata) -> Self {
        let display_path = display_path(root, &path);
        Self {
            path,
            display_path,
            byte_len: metadata.len,
            modified: metadata.modified,
        }
    }
}

struct PendingDir {
    path: AsyncVfsPath,
    gitignores: Vec<Gitignore>,
}

async fn collect_candidate_files(
    start: AsyncVfsPath,
    inherited_gitignores: Vec<Gitignore>,
    plan: &SearchPlan<'_>,
    candidates: &mut Vec<CandidateFile>,
) -> Result<(), String> {
    let mut pending = vec![PendingDir {
        path: start,
        gitignores: inherited_gitignores,
    }];

    while let Some(PendingDir {
        path,
        mut gitignores,
    }) = pending.pop()
    {
        if plan.cancel.is_cancelled() {
            return Err("grep cancelled".to_string());
        }

        if let Some(matcher) = load_gitignore_in_dir(&path).await? {
            gitignores.push(matcher);
        }

        let mut entries = directory_entries(&path).await?;
        entries.sort_by_key(|(path, _)| display_path(plan.root, path));

        for (entry_path, metadata) in entries.into_iter().rev() {
            match metadata.file_type {
                VfsFileType::File => {
                    if !is_under_vcs_dir(&entry_path)
                        && !gitignore_is_ignored(&gitignores, &entry_path, false)
                        && plan.filter.matches(plan.root, &entry_path)
                    {
                        candidates.push(CandidateFile::new(plan.root, entry_path, &metadata));
                    }
                }
                VfsFileType::Directory => {
                    if !is_vcs_dir(&entry_path)
                        && !gitignore_is_ignored(&gitignores, &entry_path, true)
                    {
                        pending.push(PendingDir {
                            path: entry_path,
                            gitignores: gitignores.clone(),
                        });
                    }
                }
            }
        }
    }

    Ok(())
}

fn sort_candidate_files_by_modified_desc(files: &mut [CandidateFile]) {
    files.sort_by(|left, right| {
        compare_modified_desc_then_path(
            left.modified.as_ref(),
            &left.display_path,
            right.modified.as_ref(),
            &right.display_path,
        )
    });
}

async fn search_one_file(
    candidate: &CandidateFile,
    plan: &SearchPlan<'_>,
    output: &mut OutputCollector,
) -> Result<bool, String> {
    let bytes = read_file_bytes(
        &candidate.path,
        candidate.byte_len.try_into().unwrap_or_default(),
    )
    .await?;
    let mut searcher = build_searcher(plan.options);
    let display = candidate.display_path.clone();

    let has_match = match plan.mode {
        GrepOutputMode::FilesWithMatches => {
            let mut sink = MatchOnlySink::new(display.clone());
            searcher
                .search_slice(plan.matcher, &bytes, &mut sink)
                .map_err(|error| error.to_string())?;
            if sink.has_match {
                output.push(display);
            }
            emit_grep_matches(plan.context, sink.matches).await?;
            sink.has_match
        }
        GrepOutputMode::Count => {
            let mut sink = CountSink::new(display.clone());
            searcher
                .search_slice(plan.matcher, &bytes, &mut sink)
                .map_err(|error| error.to_string())?;
            if sink.match_count > 0 {
                output.push(format!("{display}:{}", sink.match_count));
            }
            emit_grep_matches(plan.context, sink.matches).await?;
            sink.match_count > 0
        }
        GrepOutputMode::Content => {
            let mut sink = ContentSink {
                path: display.clone(),
                show_line_numbers: plan.options.line_numbers,
                output,
                has_match: false,
                matches: MatchUpdates::new(display.clone()),
            };
            searcher
                .search_slice(plan.matcher, &bytes, &mut sink)
                .map_err(|error| error.to_string())?;
            emit_grep_matches(plan.context, sink.matches).await?;
            sink.has_match
        }
    };

    Ok(has_match)
}

struct MatchOnlySink {
    has_match: bool,
    matches: MatchUpdates,
}

impl MatchOnlySink {
    fn new(path: String) -> Self {
        Self {
            has_match: false,
            matches: MatchUpdates::new(path),
        }
    }
}

impl Sink for MatchOnlySink {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        self.matches.push(mat);
        self.has_match = true;
        Ok(false)
    }
}

struct CountSink {
    match_count: usize,
    matches: MatchUpdates,
}

impl CountSink {
    fn new(path: String) -> Self {
        Self {
            match_count: 0,
            matches: MatchUpdates::new(path),
        }
    }
}

impl Sink for CountSink {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        self.matches.push(mat);
        self.match_count += mat.lines().count().max(1);
        Ok(true)
    }
}

struct ContentSink<'a> {
    path: String,
    show_line_numbers: bool,
    output: &'a mut OutputCollector,
    has_match: bool,
    matches: MatchUpdates,
}

impl Sink for ContentSink<'_> {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        self.has_match = true;
        self.matches.push(mat);
        for (index, line) in mat.lines().enumerate() {
            let line_number = mat.line_number().map(|number| number + index as u64);
            let output_line = render_grep_line(
                &self.path,
                GrepLineKind::Match,
                line_number,
                &bytes_to_line(line),
                self.show_line_numbers,
            );
            if !self.output.push(output_line) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        context: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        let output_line = render_grep_line(
            &self.path,
            GrepLineKind::Context,
            context.line_number(),
            &bytes_to_line(context.bytes()),
            self.show_line_numbers,
        );
        Ok(self.output.push(output_line))
    }

    fn context_break(&mut self, _searcher: &Searcher) -> Result<bool, Self::Error> {
        Ok(self.output.push("--".to_string()))
    }
}

struct MatchUpdates {
    path: String,
    matches: Vec<GrepMatchFound>,
}

impl MatchUpdates {
    fn new(path: String) -> Self {
        Self {
            path,
            matches: Vec::new(),
        }
    }

    fn push(&mut self, mat: &SinkMatch<'_>) {
        self.matches.push(GrepMatchFound {
            path: self.path.clone(),
            line_number: mat.line_number(),
            byte_offset: mat.absolute_byte_offset(),
            line_count: mat.lines().count().max(1),
        });
    }
}

async fn emit_grep_matches(context: &ToolContext, matches: MatchUpdates) -> Result<(), String> {
    for match_found in matches.matches {
        context.emit(match_found).await?;
    }
    Ok(())
}

async fn emit_grep_progress(
    context: &ToolContext,
    searched_files: usize,
    matched_files: usize,
    current: Option<&CandidateFile>,
) -> Result<(), String> {
    context
        .emit(GrepProgress {
            searched_files,
            matched_files,
            current_path: current.map(|candidate| candidate.display_path.clone()),
        })
        .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GrepLineKind {
    Match,
    Context,
}

fn render_grep_line(
    path: &str,
    kind: GrepLineKind,
    line_number: Option<u64>,
    text: &str,
    show_line_numbers: bool,
) -> String {
    let separator = match kind {
        GrepLineKind::Match => ':',
        GrepLineKind::Context => '-',
    };
    if show_line_numbers && let Some(line_number) = line_number {
        return format!("{path}{separator}{line_number}{separator}{text}");
    }
    format!("{path}{separator}{text}")
}

fn bytes_to_line(bytes: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    while text.ends_with('\n') || text.ends_with('\r') {
        text.pop();
    }
    text
}
