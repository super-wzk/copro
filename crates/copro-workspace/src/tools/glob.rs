use crate::tools::utils::{read_file_bytes, resolve_path};
use copro_agent::{CancellationToken, ToolExecutionPolicy};
use copro_api::async_trait;
use copro_harness::tools::Tool;
use futures_util::StreamExt;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::overrides::{Override, OverrideBuilder};
use schemars::JsonSchema;
use serde::Deserialize;
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use vfs::async_vfs::AsyncVfsPath;
use vfs::{VfsFileType, VfsMetadata};

pub const GLOB_TOOL_NAME: &str = "glob";

const GLOB_TOOL_DESCRIPTION: &str = concat!(
    "Find files matching a glob pattern like **/*.js or src/**/*.ts. Matches file paths, ",
    "not file contents — use grep for content search."
);

#[derive(Clone)]
pub struct GlobTool {
    root: AsyncVfsPath,
}

impl GlobTool {
    pub fn new(root: AsyncVfsPath) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &AsyncVfsPath {
        &self.root
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct GlobInput {
    /// Glob pattern to match against workspace-relative file paths, e.g. "**/*.rs".
    pub pattern: String,
    /// Directory or file to search. Defaults to the workspace root/current directory.
    #[serde(default)]
    pub path: Option<String>,
    /// Limit output lines. Defaults to 100. Use 0 for unlimited.
    #[serde(default)]
    pub head_limit: Option<usize>,
    /// Skip the first N output lines.
    #[serde(default)]
    pub offset: Option<usize>,
}

#[async_trait]
impl Tool for GlobTool {
    type Input = GlobInput;
    type Output = String;

    fn name(&self) -> &str {
        GLOB_TOOL_NAME
    }

    fn description(&self) -> &str {
        GLOB_TOOL_DESCRIPTION
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
            return Err("glob cancelled".to_string());
        }

        let filter = GlobFilter::new(&input)?;
        let search_path = input.path.as_deref().unwrap_or("");
        let mut output =
            OutputCollector::new(input.offset.unwrap_or(0), input.head_limit.unwrap_or(100));

        search_vfs(&self.root, search_path, &filter, &mut output, cancel).await?;
        Ok(output.finish())
    }
}

struct GlobFilter {
    glob: Override,
}

impl GlobFilter {
    fn new(input: &GlobInput) -> Result<Self, String> {
        let mut glob_builder = OverrideBuilder::new("");
        glob_builder
            .add(&input.pattern)
            .map_err(|error| format!("invalid glob `{}`: {error}", input.pattern))?;
        let glob = glob_builder
            .build()
            .map_err(|error| format!("invalid glob `{}`: {error}", input.pattern))?;

        Ok(Self { glob })
    }

    fn matches(&self, path: &AsyncVfsPath) -> bool {
        let display = display_path(path);
        let path = Path::new(&display);

        if !self.glob.matched(path, false).is_whitelist() {
            return false;
        }

        true
    }
}

struct OutputCollector {
    offset: usize,
    limit: usize,
    matches: Vec<GlobMatch>,
}

impl OutputCollector {
    fn new(offset: usize, limit: usize) -> Self {
        Self {
            offset,
            limit,
            matches: Vec::new(),
        }
    }

    fn push(&mut self, path: String, modified: Option<SystemTime>) {
        self.matches.push(GlobMatch { path, modified });
    }

    fn finish(mut self) -> String {
        if self.matches.is_empty() {
            return "No matches".to_string();
        }

        sort_glob_matches_by_modified_desc(&mut self.matches);
        let total = self.matches.len();
        let sort_note = sort_note_for_glob_matches(&self.matches);

        let start = self.offset.min(total);
        let end = if self.limit == 0 {
            total
        } else {
            (self.offset + self.limit).min(total)
        };
        let shown = end - start;
        let truncated = end < total;

        let paths: Vec<_> = self.matches[start..end]
            .iter()
            .map(|entry| entry.path.clone())
            .collect();

        let mut output = paths.join("\n");
        if shown == 0 {
            return format!(
                "No output: offset {} is past end ({total} file(s))",
                self.offset
            );
        }
        if let Some(sort_note) = sort_note {
            output.push('\n');
            output.push_str(sort_note);
        }
        if truncated {
            output.push_str(&format!(
                "\n{shown} of {total} files (truncated, continue with offset={})",
                start + shown,
            ));
        } else if start > 0 {
            output.push_str(&format!("\n{shown} of {total} files (offset={start})"));
        } else {
            output.push_str(&format!("\n{total} files"));
        }
        output
    }
}

struct GlobMatch {
    path: String,
    modified: Option<SystemTime>,
}

async fn search_vfs(
    root: &AsyncVfsPath,
    input_path: &str,
    filter: &GlobFilter,
    output: &mut OutputCollector,
    cancel: CancellationToken,
) -> Result<(), String> {
    let start = if input_path.is_empty() {
        root.clone()
    } else {
        resolve_path(root, input_path)?
    };

    if is_under_git_dir(&start) {
        return Ok(());
    }

    let metadata = start.metadata().await.map_err(|error| error.to_string())?;
    let is_dir = metadata.file_type == VfsFileType::Directory;
    let inherited_gitignores = load_ancestor_gitignores(root, &start).await?;

    if gitignore_is_ignored(&inherited_gitignores, &start, is_dir) {
        return Ok(());
    }

    match metadata.file_type {
        VfsFileType::File => {
            if filter.matches(&start) {
                output.push(display_path(&start), metadata.modified);
            }
        }
        VfsFileType::Directory => {
            search_directory(start, inherited_gitignores, filter, output, cancel).await?;
        }
    }

    Ok(())
}

struct PendingDir {
    path: AsyncVfsPath,
    gitignores: Vec<Gitignore>,
}

async fn search_directory(
    start: AsyncVfsPath,
    inherited_gitignores: Vec<Gitignore>,
    filter: &GlobFilter,
    output: &mut OutputCollector,
    cancel: CancellationToken,
) -> Result<(), String> {
    let mut pending = VecDeque::from([PendingDir {
        path: start,
        gitignores: inherited_gitignores,
    }]);

    while let Some(PendingDir {
        path,
        mut gitignores,
    }) = pending.pop_front()
    {
        if cancel.is_cancelled() {
            return Err("glob cancelled".to_string());
        }
        if let Some(matcher) = load_gitignore_in_dir(&path).await? {
            gitignores.push(matcher);
        }

        let mut entries = directory_entries(&path).await?;
        entries.sort_by_key(|(path, _)| display_path(path));

        for (entry_path, metadata) in entries {
            match metadata.file_type {
                VfsFileType::File => {
                    if !is_under_git_dir(&entry_path)
                        && !gitignore_is_ignored(&gitignores, &entry_path, false)
                        && filter.matches(&entry_path)
                    {
                        output.push(display_path(&entry_path), metadata.modified);
                    }
                }
                VfsFileType::Directory => {
                    if !is_git_dir(&entry_path)
                        && !gitignore_is_ignored(&gitignores, &entry_path, true)
                    {
                        pending.push_back(PendingDir {
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

fn sort_note_for_glob_matches(matches: &[GlobMatch]) -> Option<&'static str> {
    if matches.len() <= 1 {
        return None;
    }

    let missing_modified_count = matches
        .iter()
        .filter(|entry| entry.modified.is_none())
        .count();
    match missing_modified_count {
        0 => None,
        count if count == matches.len() => {
            Some("[sort: path order; modification time unavailable from VFS for matched files]")
        }
        _ => Some(
            "[sort: modification time descending; matched files without modification time sorted by path]",
        ),
    }
}

fn sort_glob_matches_by_modified_desc(matches: &mut [GlobMatch]) {
    matches.sort_by(|left, right| {
        compare_modified_desc_then_path(
            left.modified.as_ref(),
            &left.path,
            right.modified.as_ref(),
            &right.path,
        )
    });
}

fn compare_modified_desc_then_path(
    left_modified: Option<&SystemTime>,
    left_path: &str,
    right_modified: Option<&SystemTime>,
    right_path: &str,
) -> Ordering {
    let left_key = modified_sort_key(left_modified);
    let right_key = modified_sort_key(right_modified);
    right_key
        .cmp(&left_key)
        .then_with(|| left_path.cmp(right_path))
}

fn modified_sort_key(modified: Option<&SystemTime>) -> Option<Duration> {
    modified.and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
}

async fn directory_entries(
    path: &AsyncVfsPath,
) -> Result<Vec<(AsyncVfsPath, VfsMetadata)>, String> {
    let mut entries = Vec::new();
    let mut stream = path.read_dir().await.map_err(|error| error.to_string())?;
    while let Some(entry) = stream.next().await {
        let metadata = entry.metadata().await.map_err(|error| error.to_string())?;
        entries.push((entry, metadata));
    }
    Ok(entries)
}

async fn load_ancestor_gitignores(
    root: &AsyncVfsPath,
    path: &AsyncVfsPath,
) -> Result<Vec<Gitignore>, String> {
    let mut matchers = Vec::new();
    for directory in ancestor_directory_strings(path) {
        let dir = if directory.is_empty() {
            root.clone()
        } else {
            resolve_path(root, &directory)?
        };
        if let Some(matcher) = load_gitignore_in_dir(&dir).await? {
            matchers.push(matcher);
        }
    }
    Ok(matchers)
}

fn ancestor_directory_strings(path: &AsyncVfsPath) -> Vec<String> {
    let display = display_path(path);
    if display == "." {
        return Vec::new();
    }

    let Some(parent) = Path::new(&display).parent() else {
        return Vec::new();
    };

    let mut directories = vec![String::new()];
    let mut current = String::new();
    for component in parent.components() {
        let component = component.as_os_str().to_string_lossy();
        if component.is_empty() || component == "." {
            continue;
        }
        if !current.is_empty() {
            current.push('/');
        }
        current.push_str(&component);
        directories.push(current.clone());
    }
    directories
}

async fn load_gitignore_in_dir(dir: &AsyncVfsPath) -> Result<Option<Gitignore>, String> {
    let path = dir.join(".gitignore").map_err(|error| error.to_string())?;
    if !path.exists().await.map_err(|error| error.to_string())? {
        return Ok(None);
    }

    let metadata = path.metadata().await.map_err(|error| error.to_string())?;
    if metadata.file_type != VfsFileType::File {
        return Ok(None);
    }

    load_gitignore_file(&path, metadata.len).await
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

fn gitignore_is_ignored(matchers: &[Gitignore], path: &AsyncVfsPath, is_dir: bool) -> bool {
    let display = display_path(path);
    let candidate = Path::new(&display);
    let mut ignored = false;

    for matcher in matchers {
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

fn gitignore_root(path: &AsyncVfsPath) -> PathBuf {
    let parent = display_path(&path.parent());
    if parent == "." {
        PathBuf::from(".")
    } else {
        PathBuf::from(parent)
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

fn display_path(path: &AsyncVfsPath) -> String {
    let path = path.as_str().trim_start_matches('/');
    if path.is_empty() {
        ".".to_string()
    } else {
        path.to_string()
    }
}
