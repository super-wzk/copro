pub use crate::tools::utils::{CacheEntry, FileCache, FileSnapshot};
use crate::tools::utils::{read_file_bytes, resolve_path, validate_utf8};
use coox_harness::tools::{Tool, ToolContext, ToolOutput};
use copro_agent::ToolExecutionPolicy;
use copro_api::async_trait;
use copro_api::message::{ImageContent, InputContent};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;
use vfs::VfsFileType;
use vfs::async_vfs::AsyncVfsPath;

pub const READ_TOOL_NAME: &str = "read";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadToolConfig {
    pub max_line_limit: usize,
    pub line_numbers: bool,
}

impl Default for ReadToolConfig {
    fn default() -> Self {
        Self {
            max_line_limit: 2000,
            line_numbers: true,
        }
    }
}

#[derive(Clone)]
pub struct ReadTool {
    root: AsyncVfsPath,
    config: ReadToolConfig,
    description: String,
    cache: FileCache,
}

impl ReadTool {
    pub fn new(root: AsyncVfsPath) -> Self {
        Self::with_config(root, ReadToolConfig::default())
    }

    pub fn with_config(root: AsyncVfsPath, config: ReadToolConfig) -> Self {
        Self::with_cache(root, config, Arc::default())
    }

    /// Share a cache with other tools (e.g. WriteTool for read-before-write gating).
    pub fn with_cache(root: AsyncVfsPath, config: ReadToolConfig, cache: FileCache) -> Self {
        let description = format!(
            "Read a text or image file. Supports 1-based offset/limit pagination (max {max_lines} lines per call){lines_hint}.",
            max_lines = config.max_line_limit,
            lines_hint = if config.line_numbers {
                "; output is line-numbered"
            } else {
                ""
            },
        );
        Self {
            root,
            config,
            description,
            cache,
        }
    }

    pub fn root(&self) -> &AsyncVfsPath {
        &self.root
    }

    pub fn config(&self) -> &ReadToolConfig {
        &self.config
    }

    pub fn cache(&self) -> &FileCache {
        &self.cache
    }

    /// Clear the deduplication cache (e.g. at conversation boundaries).
    pub fn clear_cache(&self) {
        self.cache.lock().unwrap().clear();
    }

    /// Invalidate a single path in the cache (e.g. after a WriteTool modifies it).
    pub fn invalidate(&self, path: &str) {
        self.cache.lock().unwrap().remove(path);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct ReadInput {
    /// Path to the file to read.
    pub path: String,
    /// 1-based line number to start reading from.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Maximum number of lines to read.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutput {
    Text(String),
    Image(ImageContent),
}

impl ToolOutput for ReadOutput {
    fn into_content(self) -> Result<Vec<InputContent>, String> {
        match self {
            Self::Text(text) => Ok(vec![InputContent::Text(text)]),
            Self::Image(image) => Ok(vec![InputContent::Image(image)]),
        }
    }
}

#[async_trait]
impl Tool for ReadTool {
    type Input = ReadInput;
    type Output = ReadOutput;

    fn name(&self) -> &str {
        READ_TOOL_NAME
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::Parallel
    }

    async fn call(
        &self,
        input: Self::Input,
        _context: ToolContext,
    ) -> Result<Self::Output, String> {
        let path = resolve_path(&self.root, &input.path)?;
        let metadata = path.metadata().await.map_err(|error| error.to_string())?;
        if metadata.file_type == VfsFileType::Directory {
            return Err(format!("cannot read directory: {}", input.path));
        }

        let bytes = read_file_bytes(&path, metadata.len.try_into().unwrap_or_default()).await?;

        // Dedup: same path, bytes, offset, and limit → placeholder
        {
            let snapshot = FileSnapshot::from_metadata_and_bytes(&metadata, &bytes);
            let expected = CacheEntry {
                offset: input.offset,
                limit: input.limit,
                snapshot,
            };
            let mut cache = self.cache.lock().unwrap();
            if cache.get(&input.path) == Some(&expected) {
                return Ok(ReadOutput::Text(format!(
                    "{} — unchanged since last read",
                    input.path
                )));
            }
            cache.insert(input.path.clone(), expected);
        }

        if let Some(mime_type) = image_mime_type(&input.path) {
            return Ok(ReadOutput::Image(ImageContent::Data {
                mime_type,
                data: bytes.into(),
            }));
        }

        let text = validate_utf8(bytes, &input.path)?;
        render_text(&text, &input, &self.config).map(ReadOutput::Text)
    }
}

fn image_mime_type(path: &str) -> Option<String> {
    let extension = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    let mime = match extension.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    };
    Some(mime.to_string())
}

fn render_text(text: &str, input: &ReadInput, config: &ReadToolConfig) -> Result<String, String> {
    let offset = input.offset.unwrap_or(1);
    if offset == 0 {
        return Err("offset must be greater than or equal to 1".to_string());
    }

    let requested_limit = input.limit.unwrap_or(config.max_line_limit);
    if requested_limit == 0 {
        return Err("limit must be greater than or equal to 1".to_string());
    }

    let line_limit = requested_limit.min(config.max_line_limit.max(1));
    let lines: Vec<&str> = text.lines().collect();

    if lines.is_empty() {
        return Ok(format!("{} — no content: file is empty", input.path));
    }

    if offset > lines.len() {
        return Ok(format!(
            "{} — no content: offset {offset} is past end ({} lines)",
            input.path,
            lines.len()
        ));
    }

    let mut output = format!("{}\n", input.path);
    let number_width = digit_count(lines.len()).max(1);

    for (emitted, (index, line)) in lines.iter().enumerate().skip(offset - 1).enumerate() {
        let line_number = index + 1;
        if emitted >= line_limit {
            output.push_str(&format!(
                "\n[truncated: reached line limit; continue with offset={line_number}]"
            ));
            return Ok(output);
        }

        if emitted > 0 {
            output.push('\n');
        }
        output.push_str(&render_line(
            line_number,
            number_width,
            line,
            config.line_numbers,
        ));
    }

    Ok(output)
}

fn render_line(line_number: usize, width: usize, line: &str, line_numbers: bool) -> String {
    if line_numbers {
        format!("{line_number:>width$}: {line}")
    } else {
        line.to_string()
    }
}

/// Number of decimal digits needed to represent `n`.
pub(crate) fn digit_count(n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    let mut count = 0;
    let mut value = n;
    while value > 0 {
        value /= 10;
        count += 1;
    }
    count
}
