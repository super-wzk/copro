use crate::tools::utils::resolve_path;
use crate::tools::vfs_walk::{
    directory_entries, display_path, gitignore_is_ignored, is_under_vcs_dir, is_vcs_dir,
    load_ancestor_gitignores, load_gitignore_in_dir,
};
use copro_agent::{CancellationToken, ToolExecutionPolicy};
use copro_api::async_trait;
use copro_harness::tools::{Tool, ToolContext};
use ignore::gitignore::Gitignore;
use schemars::JsonSchema;
use serde::Deserialize;
use vfs::VfsFileType;
use vfs::async_vfs::AsyncVfsPath;

pub const LS_TOOL_NAME: &str = "ls";

const LS_TOOL_DESCRIPTION: &str = concat!(
    "List immediate files and directories at a workspace path. Directories end with '/', ",
    "results are workspace-relative, and VCS/.gitignored entries are omitted."
);

#[derive(Clone)]
pub struct LsTool {
    root: AsyncVfsPath,
}

impl LsTool {
    pub fn new(root: AsyncVfsPath) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &AsyncVfsPath {
        &self.root
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct LsInput {
    /// Directory or file to list. Defaults to the workspace root/current directory.
    #[serde(default)]
    pub path: Option<String>,
    /// Limit output lines. Defaults to 200. Use 0 for unlimited.
    #[serde(default)]
    pub head_limit: Option<usize>,
    /// Skip the first N output lines.
    #[serde(default)]
    pub offset: Option<usize>,
}

#[async_trait]
impl Tool for LsTool {
    type Input = LsInput;
    type Output = String;

    fn name(&self) -> &str {
        LS_TOOL_NAME
    }

    fn description(&self) -> &str {
        LS_TOOL_DESCRIPTION
    }

    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::Parallel
    }

    async fn call(&self, input: Self::Input, context: ToolContext) -> Result<Self::Output, String> {
        let cancel = context.cancellation().clone();
        if cancel.is_cancelled() {
            return Err("ls cancelled".to_string());
        }

        let search_path = input.path.as_deref().unwrap_or("");
        let mut output =
            OutputCollector::new(input.offset.unwrap_or(0), input.head_limit.unwrap_or(200));
        list_path(&self.root, search_path, &mut output, cancel).await?;
        Ok(output.finish())
    }
}

struct OutputCollector {
    offset: usize,
    limit: usize,
    entries: Vec<String>,
}

impl OutputCollector {
    fn new(offset: usize, limit: usize) -> Self {
        Self {
            offset,
            limit,
            entries: Vec::new(),
        }
    }

    fn push(&mut self, entry: String) {
        self.entries.push(entry);
    }

    fn finish(self) -> String {
        if self.entries.is_empty() {
            return "No entries".to_string();
        }

        let total = self.entries.len();
        let start = self.offset.min(total);
        let end = if self.limit == 0 {
            total
        } else {
            (self.offset + self.limit).min(total)
        };
        let shown = end - start;
        let truncated = end < total;

        if shown == 0 {
            return format!(
                "No output: offset {} is past end ({total} entries)",
                self.offset
            );
        }

        let mut output = self.entries[start..end].join("\n");
        if truncated {
            output.push_str(&format!(
                "\n{shown} of {total} entries (truncated, continue with offset={})",
                start + shown,
            ));
        } else if start > 0 {
            output.push_str(&format!("\n{shown} of {total} entries (offset={start})"));
        } else {
            output.push_str(&format!("\n{total} entries"));
        }
        output
    }
}

async fn list_path(
    root: &AsyncVfsPath,
    input_path: &str,
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
    let mut gitignores = load_ancestor_gitignores(root, &start).await?;

    if gitignore_is_ignored(&gitignores, &start, is_dir) {
        return Ok(());
    }

    match metadata.file_type {
        VfsFileType::File => output.push(display_path(&start)),
        VfsFileType::Directory => {
            if let Some(matcher) = load_gitignore_in_dir(&start).await? {
                gitignores.push(matcher);
            }
            list_directory(&start, &gitignores, output, cancel).await?;
        }
    }

    Ok(())
}

async fn list_directory(
    path: &AsyncVfsPath,
    gitignores: &[Gitignore],
    output: &mut OutputCollector,
    cancel: CancellationToken,
) -> Result<(), String> {
    if cancel.is_cancelled() {
        return Err("ls cancelled".to_string());
    }

    let mut entries = directory_entries(path).await?;
    entries.sort_by_key(|(path, metadata)| sort_key(path, metadata.file_type));

    for (entry_path, metadata) in entries {
        if cancel.is_cancelled() {
            return Err("ls cancelled".to_string());
        }

        let is_dir = metadata.file_type == VfsFileType::Directory;
        if is_under_vcs_dir(&entry_path)
            || is_vcs_dir(&entry_path)
            || gitignore_is_ignored(gitignores, &entry_path, is_dir)
        {
            continue;
        }

        output.push(render_entry(&entry_path, metadata.file_type));
    }

    Ok(())
}

fn sort_key(path: &AsyncVfsPath, file_type: VfsFileType) -> (u8, String) {
    let kind = match file_type {
        VfsFileType::Directory => 0,
        VfsFileType::File => 1,
    };
    (kind, display_path(path))
}

fn render_entry(path: &AsyncVfsPath, file_type: VfsFileType) -> String {
    let mut entry = display_path(path);
    if file_type == VfsFileType::Directory {
        entry.push('/');
    }
    entry
}
