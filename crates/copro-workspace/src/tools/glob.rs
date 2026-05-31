use crate::tools::utils::resolve_path;
use crate::tools::vfs_walk::{
    compare_modified_desc_then_path, directory_entries, display_path, gitignore_is_ignored,
    is_under_vcs_dir, is_vcs_dir, load_ancestor_gitignores, load_gitignore_in_dir,
};
use copro_agent::{CancellationToken, ToolExecutionPolicy};
use copro_api::async_trait;
use copro_harness::tools::{Tool, ToolContext, ToolUpdatePayload};
use ignore::gitignore::Gitignore;
use ignore::overrides::{Override, OverrideBuilder};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::Path;
use std::time::SystemTime;
use vfs::VfsFileType;
use vfs::async_vfs::AsyncVfsPath;

pub const GLOB_TOOL_NAME: &str = "glob";

const GLOB_TOOL_DESCRIPTION: &str = concat!(
    "Find files matching a glob pattern like **/*.js or src/**/*.ts. Matches file paths, ",
    "not file contents — use grep for content search. Set include_ignored to true to include ",
    ".gitignored files. VCS directories are always skipped."
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobProgress {
    pub searched_directories: usize,
    pub searched_files: usize,
    pub matched_files: usize,
    pub current_path: Option<String>,
}

impl ToolUpdatePayload for GlobProgress {
    const KIND: &'static str = "glob.progress";
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
    /// Include files ignored by .gitignore rules. VCS directories are always skipped.
    #[serde(default)]
    pub include_ignored: bool,
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

    async fn call(&self, input: Self::Input, context: ToolContext) -> Result<Self::Output, String> {
        let cancel = context.cancellation().clone();
        if cancel.is_cancelled() {
            return Err("glob cancelled".to_string());
        }

        let filter = GlobFilter::new(&input)?;
        let search_path = input.path.as_deref().unwrap_or("");
        let mut output =
            OutputCollector::new(input.offset.unwrap_or(0), input.head_limit.unwrap_or(100));

        search_vfs(
            &self.root,
            search_path,
            &context,
            &filter,
            input.include_ignored,
            &mut output,
            cancel,
        )
        .await?;
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
    context: &ToolContext,
    filter: &GlobFilter,
    include_ignored: bool,
    output: &mut OutputCollector,
    cancel: CancellationToken,
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
    let inherited_gitignores = if include_ignored {
        Vec::new()
    } else {
        load_ancestor_gitignores(root, &start).await?
    };

    if !include_ignored && gitignore_is_ignored(&inherited_gitignores, &start, is_dir) {
        return Ok(());
    }

    match metadata.file_type {
        VfsFileType::File => {
            let mut progress = GlobProgressState::default();
            progress.searched_files += 1;
            if filter.matches(&start) {
                let path = display_path(&start);
                output.push(path, metadata.modified);
                progress.matched_files += 1;
            }
            emit_glob_progress(context, &progress, Some(&start)).await?;
            emit_glob_progress(context, &progress, None).await?;
        }
        VfsFileType::Directory => {
            let mut progress = GlobProgressState::default();
            search_directory(
                start,
                inherited_gitignores,
                context,
                filter,
                include_ignored,
                output,
                cancel,
                &mut progress,
            )
            .await?;
            emit_glob_progress(context, &progress, None).await?;
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
    context: &ToolContext,
    filter: &GlobFilter,
    include_ignored: bool,
    output: &mut OutputCollector,
    cancel: CancellationToken,
    progress: &mut GlobProgressState,
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
        if !include_ignored && let Some(matcher) = load_gitignore_in_dir(&path).await? {
            gitignores.push(matcher);
        }
        progress.searched_directories += 1;

        let mut entries = directory_entries(&path).await?;
        entries.sort_by_key(|(path, _)| display_path(path));

        for (entry_path, metadata) in entries {
            match metadata.file_type {
                VfsFileType::File => {
                    if !is_under_vcs_dir(&entry_path)
                        && (include_ignored
                            || !gitignore_is_ignored(&gitignores, &entry_path, false))
                    {
                        progress.searched_files += 1;
                        if filter.matches(&entry_path) {
                            let path = display_path(&entry_path);
                            output.push(path, metadata.modified);
                            progress.matched_files += 1;
                        }
                    }
                }
                VfsFileType::Directory => {
                    if !is_vcs_dir(&entry_path)
                        && (include_ignored
                            || !gitignore_is_ignored(&gitignores, &entry_path, true))
                    {
                        pending.push_back(PendingDir {
                            path: entry_path,
                            gitignores: gitignores.clone(),
                        });
                    }
                }
            }
        }
        emit_glob_progress(context, progress, Some(&path)).await?;
    }

    Ok(())
}

#[derive(Default)]
struct GlobProgressState {
    searched_directories: usize,
    searched_files: usize,
    matched_files: usize,
}

async fn emit_glob_progress(
    context: &ToolContext,
    progress: &GlobProgressState,
    current: Option<&AsyncVfsPath>,
) -> Result<(), String> {
    context
        .emit(GlobProgress {
            searched_directories: progress.searched_directories,
            searched_files: progress.searched_files,
            matched_files: progress.matched_files,
            current_path: current.map(display_path),
        })
        .await
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
