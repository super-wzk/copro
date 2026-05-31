use crate::tools::read::READ_TOOL_NAME;
use crate::tools::utils::{CacheEntry, FileCache, FileSnapshot, read_file_bytes, resolve_path};
use async_std::io::WriteExt;
use copro_agent::ToolExecutionPolicy;
use copro_api::async_trait;
use copro_harness::tools::{Tool, ToolContext};
use schemars::JsonSchema;
use serde::Deserialize;
use vfs::VfsFileType;
use vfs::async_vfs::AsyncVfsPath;

pub const WRITE_TOOL_NAME: &str = "write";

const WRITE_TOOL_DESCRIPTION: &str = "Create or overwrite a text file. Creating a new file is allowed directly; overwriting an existing file requires a known snapshot from a prior read or successful write, and the file must not have changed since that snapshot. Set create_dirs: true to automatically create parent directories.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteToolConfig {
    pub description: String,
}

impl Default for WriteToolConfig {
    fn default() -> Self {
        Self {
            description: WRITE_TOOL_DESCRIPTION.to_string(),
        }
    }
}

#[derive(Clone)]
pub struct WriteTool {
    root: AsyncVfsPath,
    config: WriteToolConfig,
    cache: FileCache,
}

impl WriteTool {
    pub fn new(root: AsyncVfsPath) -> Self {
        Self::with_cache_and_config(root, None, WriteToolConfig::default())
    }

    pub fn with_cache(root: AsyncVfsPath, cache: FileCache) -> Self {
        Self::with_cache_and_config(root, Some(cache), WriteToolConfig::default())
    }

    fn with_cache_and_config(
        root: AsyncVfsPath,
        cache: Option<FileCache>,
        config: WriteToolConfig,
    ) -> Self {
        Self {
            root,
            config,
            cache: cache.unwrap_or_default(),
        }
    }

    pub fn cache(&self) -> &FileCache {
        &self.cache
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct WriteToolInput {
    /// Path of the file to write, relative to workspace root.
    pub path: String,
    /// Complete UTF-8 text content to write to the file.
    pub content: String,
    /// When true, create any missing parent directories before writing.
    #[serde(default)]
    pub create_dirs: bool,
}

#[async_trait]
impl Tool for WriteTool {
    type Input = WriteToolInput;
    type Output = String;

    fn name(&self) -> &str {
        WRITE_TOOL_NAME
    }

    fn description(&self) -> &str {
        &self.config.description
    }

    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::Serial
    }

    async fn call(
        &self,
        input: Self::Input,
        _context: ToolContext,
    ) -> Result<Self::Output, String> {
        let path = resolve_path(&self.root, &input.path)?;

        let exists = path.exists().await.map_err(|error| error.to_string())?;
        if exists {
            validate_existing_file(&path, &input.path, &self.cache).await?;
        }

        if input.create_dirs {
            path.parent()
                .create_dir_all()
                .await
                .map_err(|error| error.to_string())?;
        }

        let content_bytes = input.content.into_bytes();
        let byte_count = content_bytes.len();

        let mut file = path
            .create_file()
            .await
            .map_err(|error| error.to_string())?;
        file.write_all(&content_bytes)
            .await
            .map_err(|error| error.to_string())?;
        file.flush().await.map_err(|error| error.to_string())?;
        drop(file);

        let metadata = path.metadata().await.map_err(|error| error.to_string())?;
        let snapshot = FileSnapshot::from_metadata_and_bytes(&metadata, &content_bytes);

        self.cache.lock().unwrap().insert(
            input.path.clone(),
            CacheEntry {
                offset: None,
                limit: None,
                snapshot,
            },
        );

        let action = if exists { "overwritten" } else { "created" };
        Ok(format!(
            "{}: {byte_count} byte(s) written ({action})",
            input.path
        ))
    }
}

async fn validate_existing_file(
    path: &AsyncVfsPath,
    input_path: &str,
    cache: &FileCache,
) -> Result<(), String> {
    let metadata = path.metadata().await.map_err(|error| error.to_string())?;
    if metadata.file_type == VfsFileType::Directory {
        return Err(format!("cannot write directory: {input_path}"));
    }

    let known_snapshot = cache
        .lock()
        .unwrap()
        .get(input_path)
        .map(|entry| entry.snapshot.clone());
    if known_snapshot.is_none() {
        return Err(format!(
            "{input_path} must be read with `{READ_TOOL_NAME}` or previously written with `{WRITE_TOOL_NAME}` before writing"
        ));
    }

    let current_bytes = read_file_bytes(path, metadata.len.try_into().unwrap_or_default()).await?;
    let current_snapshot = FileSnapshot::from_metadata_and_bytes(&metadata, &current_bytes);

    if known_snapshot.as_ref() != Some(&current_snapshot) {
        return Err(format!(
            "{input_path} changed since it was last read or written; read it again with `{READ_TOOL_NAME}` before writing"
        ));
    }

    Ok(())
}
