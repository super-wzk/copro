use crate::tools::utils::{read_file_bytes, resolve_path};
use copro_agent::{CancellationToken, ToolExecutionPolicy};
use copro_api::async_trait;
use copro_harness::tools::Tool;
use futures_util::StreamExt;
use grep::regex::RegexMatcherBuilder;
use grep::searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::overrides::{Override, OverrideBuilder};
use ignore::types::{Types, TypesBuilder};
use schemars::JsonSchema;
use serde::Deserialize;
use std::io;
use std::path::{Path, PathBuf};
use vfs::async_vfs::AsyncVfsPath;
use vfs::VfsFileType;

pub const GREP_TOOL_NAME: &str = "grep";

const GREP_TOOL_DESCRIPTION: &str = "Search files recursively with ripgrep-compatible regular expressions. Supports glob/type filters, context lines, line numbers, case-insensitive search, count/files/content output modes, and multiline matching.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepToolConfig {
    pub description: String,
    pub default_head_limit: usize,
}

impl Default for GrepToolConfig {
    fn default() -> Self {
        Self {
            description: GREP_TOOL_DESCRIPTION.to_string(),
            default_head_limit: 250,
        }
    }
}

#[derive(Clone)]
pub struct GrepTool {
    root: AsyncVfsPath,
    config: GrepToolConfig,
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
        &self.config.description
    }

    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::Parallel
    }

    async fn call(
        &self,
        input: Self::Input,
        cancel: CancellationToken,
    ) -> Result<Self::Output, String> {
        if cancel.is_cancelled() {
            return Err("grep cancelled".to_string());
        }

        let matcher = build_matcher(&input)?;
        let filter = GrepFilter::new(&input)?;
        let gitignore = GitignoreSet::load(&self.root).await?;
        let search_path = input.path.as_deref().unwrap_or("");
        let candidates = candidate_files(&self.root, search_path, &filter, &gitignore).await?;
        let search_options = SearchOptions::from_input(&input);
        let mut results = Vec::new();

        for path in candidates {
            if cancel.is_cancelled() {
                return Err("grep cancelled".to_string());
            }

            let metadata = path.metadata().await.map_err(|error| error.to_string())?;
            let bytes = read_file_bytes(&path, metadata.len.try_into().unwrap_or_default()).await?;
            let mut searcher = build_searcher(&search_options);
            let mut sink = GrepSink::default();
            searcher
                .search_slice(&matcher, &bytes, &mut sink)
                .map_err(|error| error.to_string())?;

            if sink.match_count > 0 {
                results.push(FileSearchResult {
                    path: display_path(&path),
                    match_count: sink.match_count,
                    lines: sink.lines,
                });
            }
        }

        render_results(&results, &input, &self.config)
    }
}

fn build_matcher(input: &GrepInput) -> Result<grep::regex::RegexMatcher, String> {
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

    fn matches(&self, path: &AsyncVfsPath) -> bool {
        let display = display_path(path);
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

#[derive(Debug, Default)]
struct GitignoreSet {
    matchers: Vec<Gitignore>,
}

impl GitignoreSet {
    async fn load(root: &AsyncVfsPath) -> Result<Self, String> {
        let mut matchers = Vec::new();
        let mut dirs = vec![root.clone()];

        while let Some(dir) = dirs.pop() {
            let mut entries = Vec::new();
            let mut stream = dir.read_dir().await.map_err(|error| error.to_string())?;
            while let Some(entry) = stream.next().await {
                entries.push(entry);
            }
            entries.sort_by_key(display_path);

            for path in entries {
                let metadata = path.metadata().await.map_err(|error| error.to_string())?;
                match metadata.file_type {
                    VfsFileType::Directory => {
                        if !is_git_dir(&path) {
                            dirs.push(path);
                        }
                    }
                    VfsFileType::File => {
                        if path.filename() == ".gitignore"
                            && let Some(matcher) = load_gitignore_file(&path, metadata.len).await?
                        {
                            matchers.push(matcher);
                        }
                    }
                }
            }
        }

        matchers.sort_by_key(|matcher| path_depth(matcher.path()));
        Ok(Self { matchers })
    }

    fn is_ignored(&self, path: &AsyncVfsPath, is_dir: bool) -> bool {
        let display = display_path(path);
        let candidate = Path::new(&display);
        let mut ignored = false;

        for matcher in &self.matchers {
            if !path_is_under(candidate, matcher.path()) {
                continue;
            }

            match matcher.matched_path_or_any_parents(candidate, is_dir) {
                ignore::Match::None => {}
                ignore::Match::Ignore(_) => ignored = true,
                ignore::Match::Whitelist(_) => ignored = false,
            }
        }

        ignored
    }
}

async fn load_gitignore_file(
    path: &AsyncVfsPath,
    byte_len: u64,
) -> Result<Option<Gitignore>, String> {
    let bytes = read_file_bytes(path, byte_len.try_into().unwrap_or_default()).await?;
    let text = String::from_utf8_lossy(&bytes);
    let source = PathBuf::from(display_path(path));
    let mut builder = GitignoreBuilder::new(gitignore_root(path));

    for (index, line) in text.lines().enumerate() {
        let line = if index == 0 {
            line.trim_start_matches('\u{feff}')
        } else {
            line
        };
        builder
            .add_line(Some(source.clone()), line)
            .map_err(|error| {
                format!(
                    "invalid .gitignore rule in {}:{}: {error}",
                    source.display(),
                    index + 1
                )
            })?;
    }

    let matcher = builder
        .build()
        .map_err(|error| format!("invalid .gitignore {}: {error}", source.display()))?;
    Ok((!matcher.is_empty()).then_some(matcher))
}

fn gitignore_root(path: &AsyncVfsPath) -> PathBuf {
    let parent = display_path(&path.parent());
    if parent == "." {
        PathBuf::from(".")
    } else {
        PathBuf::from(parent)
    }
}

fn path_depth(path: &Path) -> usize {
    if path == Path::new(".") || path.as_os_str().is_empty() {
        0
    } else {
        path.components().count()
    }
}

fn path_is_under(path: &Path, root: &Path) -> bool {
    root == Path::new(".") || root.as_os_str().is_empty() || path == root || path.starts_with(root)
}

fn is_git_dir(path: &AsyncVfsPath) -> bool {
    path.filename() == ".git"
}

fn is_under_git_dir(path: &AsyncVfsPath) -> bool {
    Path::new(&display_path(path))
        .components()
        .any(|component| component.as_os_str() == ".git")
}

async fn candidate_files(
    root: &AsyncVfsPath,
    input_path: &str,
    filter: &GrepFilter,
    gitignore: &GitignoreSet,
) -> Result<Vec<AsyncVfsPath>, String> {
    let start = if input_path.is_empty() {
        root.clone()
    } else {
        resolve_path(root, input_path)?
    };
    if is_under_git_dir(&start) {
        return Ok(Vec::new());
    }

    let metadata = start.metadata().await.map_err(|error| error.to_string())?;

    let mut files = Vec::new();
    match metadata.file_type {
        VfsFileType::File => {
            if !gitignore.is_ignored(&start, false) && filter.matches(&start) {
                files.push(start);
            }
        }
        VfsFileType::Directory => {
            let mut dirs = vec![start];
            while let Some(dir) = dirs.pop() {
                let mut entries = Vec::new();
                let mut stream = dir.read_dir().await.map_err(|error| error.to_string())?;
                while let Some(entry) = stream.next().await {
                    entries.push(entry);
                }
                entries.sort_by_key(display_path);

                for path in entries {
                    let metadata = path.metadata().await.map_err(|error| error.to_string())?;
                    match metadata.file_type {
                        VfsFileType::File => {
                            if !is_under_git_dir(&path)
                                && !gitignore.is_ignored(&path, false)
                                && filter.matches(&path)
                            {
                                files.push(path);
                            }
                        }
                        VfsFileType::Directory => {
                            if !is_git_dir(&path) && !gitignore.is_ignored(&path, true) {
                                dirs.push(path);
                            }
                        }
                    }
                }
            }
        }
    }

    files.sort_by_key(display_path);
    Ok(files)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSearchResult {
    path: String,
    match_count: usize,
    lines: Vec<GrepLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GrepLine {
    kind: GrepLineKind,
    line_number: Option<u64>,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GrepLineKind {
    Match,
    Context,
    Break,
}

#[derive(Debug, Default)]
struct GrepSink {
    lines: Vec<GrepLine>,
    match_count: usize,
}

impl Sink for GrepSink {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        let mut line_count = 0usize;
        for (index, line) in mat.lines().enumerate() {
            let line_number = mat.line_number().map(|number| number + index as u64);
            self.lines.push(GrepLine {
                kind: GrepLineKind::Match,
                line_number,
                text: bytes_to_line(line),
            });
            line_count += 1;
        }
        self.match_count += line_count.max(1);
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        context: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        self.lines.push(GrepLine {
            kind: GrepLineKind::Context,
            line_number: context.line_number(),
            text: bytes_to_line(context.bytes()),
        });
        Ok(true)
    }

    fn context_break(&mut self, _searcher: &Searcher) -> Result<bool, Self::Error> {
        self.lines.push(GrepLine {
            kind: GrepLineKind::Break,
            line_number: None,
            text: String::new(),
        });
        Ok(true)
    }
}

fn render_results(
    results: &[FileSearchResult],
    input: &GrepInput,
    config: &GrepToolConfig,
) -> Result<String, String> {
    let show_line_numbers = SearchOptions::from_input(input).line_numbers;
    let lines = match input.output_mode {
        GrepOutputMode::FilesWithMatches => results
            .iter()
            .map(|result| result.path.clone())
            .collect::<Vec<_>>(),
        GrepOutputMode::Count => results
            .iter()
            .map(|result| format!("{}:{}", result.path, result.match_count))
            .collect::<Vec<_>>(),
        GrepOutputMode::Content => results
            .iter()
            .flat_map(|result| {
                result
                    .lines
                    .iter()
                    .map(|line| render_grep_line(&result.path, line, show_line_numbers))
            })
            .collect::<Vec<_>>(),
    };

    Ok(render_limited_lines(
        lines,
        input.offset.unwrap_or(0),
        input.head_limit.unwrap_or(config.default_head_limit),
    ))
}

fn render_grep_line(path: &str, line: &GrepLine, show_line_numbers: bool) -> String {
    match line.kind {
        GrepLineKind::Break => "--".to_string(),
        GrepLineKind::Match | GrepLineKind::Context => {
            let separator = match line.kind {
                GrepLineKind::Match => ':',
                GrepLineKind::Context => '-',
                GrepLineKind::Break => unreachable!(),
            };
            if show_line_numbers && let Some(line_number) = line.line_number {
                return format!("{path}{separator}{line_number}{separator}{}", line.text);
            }
            format!("{path}{separator}{}", line.text)
        }
    }
}

fn render_limited_lines(lines: Vec<String>, offset: usize, head_limit: usize) -> String {
    if lines.is_empty() {
        return "No matches".to_string();
    }

    if offset >= lines.len() {
        return format!(
            "No output: offset {offset} is past end ({} line(s))",
            lines.len()
        );
    }

    let end = if head_limit == 0 {
        lines.len()
    } else {
        (offset + head_limit).min(lines.len())
    };
    let mut output = lines[offset..end].join("\n");
    if head_limit > 0 && end < lines.len() {
        output.push_str(&format!(
            "\n[truncated: reached head_limit; continue with offset={end}]"
        ));
    }
    output
}

fn bytes_to_line(bytes: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    while text.ends_with('\n') || text.ends_with('\r') {
        text.pop();
    }
    text
}

fn display_path(path: &AsyncVfsPath) -> String {
    let path = path.as_str().trim_start_matches('/');
    if path.is_empty() {
        ".".to_string()
    } else {
        path.to_string()
    }
}
