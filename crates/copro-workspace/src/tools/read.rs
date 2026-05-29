use async_std::io::ReadExt;
use copro_agent::{CancellationToken, ToolExecutionPolicy};
use copro_api::async_trait;
use copro_api::message::{ImageContent, InputContent};
use copro_harness::tools::{Tool, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use vfs::VfsFileType;
use vfs::async_vfs::AsyncVfsPath;

pub const READ_TOOL_NAME: &str = "read";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadToolConfig {
    pub max_line_limit: usize,
    pub max_text_bytes: usize,
    pub line_numbers: bool,
}

impl Default for ReadToolConfig {
    fn default() -> Self {
        Self {
            max_line_limit: 2000,
            max_text_bytes: 50 * 1024,
            line_numbers: true,
        }
    }
}

#[derive(Clone)]
pub struct ReadTool {
    root: AsyncVfsPath,
    config: ReadToolConfig,
    description: String,
}

impl ReadTool {
    pub fn new(root: AsyncVfsPath) -> Self {
        Self::with_config(root, ReadToolConfig::default())
    }

    pub fn with_config(root: AsyncVfsPath, config: ReadToolConfig) -> Self {
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
        }
    }

    pub fn root(&self) -> &AsyncVfsPath {
        &self.root
    }

    pub fn config(&self) -> &ReadToolConfig {
        &self.config
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
    fn into_tool_result_content(self) -> Result<Vec<InputContent>, String> {
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
        _cancel: CancellationToken,
    ) -> Result<Self::Output, String> {
        let path = self
            .root
            .join(&input.path)
            .map_err(|error| error.to_string())?;
        let metadata = path.metadata().await.map_err(|error| error.to_string())?;
        if metadata.file_type == VfsFileType::Directory {
            return Err(format!("cannot read directory: {}", input.path));
        }

        let mut file = path.open_file().await.map_err(|error| error.to_string())?;
        let mut bytes = Vec::with_capacity(metadata.len.try_into().unwrap_or_default());
        file.read_to_end(&mut bytes)
            .await
            .map_err(|error| error.to_string())?;

        if let Some(mime_type) = image_mime_type(&input.path) {
            return Ok(ReadOutput::Image(ImageContent::Data {
                mime_type,
                data: bytes,
            }));
        }

        let text = String::from_utf8(bytes)
            .map_err(|_| format!("file is not valid UTF-8 text: {}", input.path))?;
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

    let max_line_limit = config.max_line_limit.max(1);
    let line_limit = requested_limit.min(max_line_limit);
    let max_text_bytes = config.max_text_bytes.max(1);
    let lines: Vec<&str> = text.lines().collect();

    if lines.is_empty() {
        return Ok(format!("[read] {} — no content: file is empty", input.path));
    }

    if offset > lines.len() {
        return Ok(format!(
            "[read] {} — no content: offset {offset} is past end ({} lines)",
            input.path,
            lines.len()
        ));
    }

    let mut output = format!("[read] {}\n", input.path);
    let number_width = digit_count(lines.len()).max(1);
    let mut truncation = None;

    for (emitted, (index, line)) in lines.iter().enumerate().skip(offset - 1).enumerate() {
        let line_number = index + 1;
        if emitted >= line_limit {
            truncation = Some(Truncation {
                reason: "reached line limit",
                next_offset: line_number,
            });
            break;
        }

        let rendered_line = render_line(line_number, number_width, line, config.line_numbers);
        let separator_bytes = usize::from(emitted > 0);
        let required_bytes = separator_bytes + rendered_line.len();

        if output.len() + required_bytes > max_text_bytes {
            if emitted == 0 {
                let remaining = max_text_bytes.saturating_sub(output.len());
                output.push_str(truncate_to_char_boundary(&rendered_line, remaining));
                truncation = Some(Truncation {
                    reason: "reached byte limit; current line was partially shown",
                    next_offset: line_number + 1,
                });
            } else {
                truncation = Some(Truncation {
                    reason: "reached byte limit",
                    next_offset: line_number,
                });
            }
            break;
        }

        if emitted > 0 {
            output.push('\n');
        }
        output.push_str(&rendered_line);
    }

    if let Some(truncation) = truncation {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&format!(
            "[truncated: {}; continue with offset={}]",
            truncation.reason, truncation.next_offset
        ));
    }

    Ok(output)
}

fn render_line(line_number: usize, width: usize, line: &str, line_numbers: bool) -> String {
    if line_numbers {
        format!("{line_number:>width$}\t{line}")
    } else {
        line.to_string()
    }
}

/// Number of decimal digits needed to represent `n`.
fn digit_count(n: usize) -> usize {
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

fn truncate_to_char_boundary(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }

    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

struct Truncation {
    reason: &'static str,
    next_offset: usize,
}
