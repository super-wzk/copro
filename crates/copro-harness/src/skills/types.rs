use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Lightweight skill metadata that is safe to keep in the model context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
}

impl SkillSummary {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }
}

/// Fully loaded skill instructions and their root directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillDocument {
    pub summary: SkillSummary,
    /// Skill root used to resolve relative file references such as `scripts/run.sh`.
    pub root: PathBuf,
    /// Full model-readable skill instructions. A parser may choose to include the
    /// whole `SKILL.md` or only the markdown body after frontmatter.
    pub content: String,
}

impl SkillDocument {
    pub fn new(
        summary: SkillSummary,
        root: impl Into<PathBuf>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            summary,
            root: root.into(),
            content: content.into(),
        }
    }
}
