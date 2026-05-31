use crate::tools::read::READ_TOOL_NAME;
use crate::tools::read::digit_count;
use crate::tools::utils::{FileCache, read_file_bytes, resolve_path, validate_utf8};
use async_std::io::WriteExt;
use copro_agent::ToolExecutionPolicy;
use copro_api::async_trait;
use copro_harness::tools::{Tool, ToolContext, ToolUpdatePayload};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use vfs::async_vfs::AsyncVfsPath;

use similar::{ChangeTag, TextDiff};

pub const EDIT_TOOL_NAME: &str = "edit";

const EDIT_TOOL_DESCRIPTION: &str =
    "Edit a text file. Requires the file to have been read first in this conversation.";

#[derive(Clone)]
pub struct EditTool {
    root: AsyncVfsPath,
    cache: FileCache,
}

impl EditTool {
    pub fn new(root: AsyncVfsPath) -> Self {
        Self {
            root,
            cache: FileCache::default(),
        }
    }

    pub fn with_cache(root: AsyncVfsPath, cache: FileCache) -> Self {
        Self { root, cache }
    }

    pub fn cache(&self) -> &FileCache {
        &self.cache
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditMatchFound {
    pub path: String,
    pub line_number: u64,
}

impl ToolUpdatePayload for EditMatchFound {
    const KIND: &'static str = "edit.match_found";
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
pub struct EditToolInput {
    /// Path of the file to edit, relative to workspace root.
    pub path: String,
    /// The exact text to search for and replace.
    pub old_string: String,
    /// The text to replace old_string with (use empty string for deletion).
    pub new_string: String,
    /// Replace all occurrences. When false (default), replaces only the first match.
    #[serde(default)]
    pub replace_all: bool,
}

#[async_trait]
impl Tool for EditTool {
    type Input = EditToolInput;
    type Output = String;

    fn name(&self) -> &str {
        EDIT_TOOL_NAME
    }

    fn description(&self) -> &str {
        EDIT_TOOL_DESCRIPTION
    }

    fn execution_policy(&self) -> ToolExecutionPolicy {
        ToolExecutionPolicy::Serial
    }

    async fn call(&self, input: Self::Input, context: ToolContext) -> Result<Self::Output, String> {
        // Gate: file must have been read first
        {
            let cache_guard = self.cache.lock().unwrap();
            if !cache_guard.contains_key(&input.path) {
                return Err(format!(
                    "{} must be read with `{}` before editing",
                    input.path, READ_TOOL_NAME
                ));
            }
        }

        let path = resolve_path(&self.root, &input.path)?;

        // Read current file content
        let bytes = read_file_bytes(&path, 0).await?;

        let content = validate_utf8(bytes, &input.path)?;

        let matches = find_matches(&content, &input.old_string);
        let occurrence_count = matches.len();
        if occurrence_count == 0 {
            return Err(format!(
                "{}: `{}` not found in file",
                input.path,
                truncate_for_error(&input.old_string)
            ));
        }
        for line_number in matches.iter().map(|mat| mat.line_number) {
            context
                .emit(EditMatchFound {
                    path: input.path.clone(),
                    line_number,
                })
                .await?;
        }
        if !input.replace_all && occurrence_count > 1 {
            return Err(format!(
                "{}: `{}` appears {occurrence_count} times; use `replace_all: true` or include more surrounding context to make it unique",
                input.path,
                truncate_for_error(&input.old_string)
            ));
        }

        let replacements = replace_text(
            &content,
            &input.old_string,
            &input.new_string,
            input.replace_all,
        );

        // Write modified content back
        let mut file = path
            .create_file()
            .await
            .map_err(|error| error.to_string())?;
        file.write_all(replacements.output.as_bytes())
            .await
            .map_err(|error| error.to_string())?;
        file.flush().await.map_err(|error| error.to_string())?;
        drop(file);

        let diff = format_diff_with_line_numbers(&content, &replacements.output);

        // Invalidate cache so next read picks up new content
        self.cache.lock().unwrap().remove::<String>(&input.path);

        Ok(format!(
            "{}: {} replacement(s)\n{diff}",
            input.path, replacements.count
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EditMatch {
    line_number: u64,
}

struct Replacement {
    output: String,
    count: usize,
}

fn format_diff_with_line_numbers(old: &str, new: &str) -> String {
    const CONTEXT_RADIUS: usize = 4;
    let old_line_count = old.lines().count();
    let new_line_count = new.lines().count();
    let width = digit_count(old_line_count.max(new_line_count)).max(1);
    let diff = TextDiff::from_lines(old, new);

    let changes: Vec<(ChangeTag, String)> = diff
        .iter_all_changes()
        .map(|c| (c.tag(), c.to_string()))
        .collect();

    let is_change = |i: usize| changes[i].0 != ChangeTag::Equal;
    let mut show = vec![false; changes.len()];
    for (i, slot) in show.iter_mut().enumerate() {
        if is_change(i) {
            *slot = true;
        } else {
            let lo = i.saturating_sub(CONTEXT_RADIUS);
            let hi = (i + CONTEXT_RADIUS).min(changes.len().saturating_sub(1));
            *slot = (lo..=hi).any(&is_change);
        }
    }

    let mut output = String::new();
    let mut old_line = 1usize;
    let mut new_line = 1usize;
    let mut skipping = false;

    for (i, (tag, line)) in changes.iter().enumerate() {
        if !show[i] {
            if !skipping && i > 0 && show[i - 1] {
                skipping = true;
                output.push_str(&format!("{:>width$} ...\n", ""));
            }
            match tag {
                ChangeTag::Delete => old_line += 1,
                ChangeTag::Insert => new_line += 1,
                ChangeTag::Equal => {
                    old_line += 1;
                    new_line += 1;
                }
            }
            continue;
        }
        skipping = false;

        let (prefix, displayed) = match tag {
            ChangeTag::Delete => {
                let n = old_line;
                ('-', n)
            }
            ChangeTag::Insert => {
                let n = new_line;
                ('+', n)
            }
            ChangeTag::Equal => {
                let n = old_line;
                (' ', n)
            }
        };
        output.push_str(&format!("{prefix}{displayed:>width$} {line}"));
        match tag {
            ChangeTag::Delete => old_line += 1,
            ChangeTag::Insert => new_line += 1,
            ChangeTag::Equal => {
                old_line += 1;
                new_line += 1;
            }
        }
    }
    output
}

fn find_matches(content: &str, old: &str) -> Vec<EditMatch> {
    if old.is_empty() {
        return Vec::new();
    }

    let mut matches = Vec::new();
    let mut cursor = 0usize;
    let mut line_number = 1u64;
    while let Some(pos) = content[cursor..].find(old) {
        let match_pos = cursor + pos;
        line_number += content[cursor..match_pos]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count() as u64;
        matches.push(EditMatch { line_number });
        cursor = match_pos + old.len();
    }
    matches
}

fn replace_text(content: &str, old: &str, new: &str, replace_all: bool) -> Replacement {
    if old.is_empty() {
        return Replacement {
            output: content.to_string(),
            count: 0,
        };
    }

    let mut output = String::with_capacity(content.len());
    let mut count = 0usize;
    let mut cursor = 0usize;

    while let Some(pos) = content[cursor..].find(old) {
        let match_pos = cursor + pos;
        output.push_str(&content[cursor..match_pos]);
        output.push_str(new);
        cursor = match_pos + old.len();
        count += 1;

        if !replace_all {
            // Append the rest and return
            output.push_str(&content[cursor..]);
            return Replacement { output, count };
        }
    }

    output.push_str(&content[cursor..]);
    Replacement { output, count }
}

fn truncate_for_error(s: &str) -> String {
    if s.len() <= 40 {
        s.to_string()
    } else {
        let end = s
            .char_indices()
            .take(41)
            .last()
            .map(|(i, _)| i)
            .unwrap_or(40);
        format!("{}…", &s[..end])
    }
}
