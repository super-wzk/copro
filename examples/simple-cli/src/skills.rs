use coox_harness::skills::{SkillDocument, SkillStore, SkillSummary};
use copro_api::async_trait;
use copro_api::error::{Error, Result};
use std::path::PathBuf;

pub struct ExampleSkillStore {
    skills: Vec<SkillDocument>,
}

impl ExampleSkillStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            skills: vec![
                SkillDocument::new(
                    SkillSummary::new(
                        "accurate-calculation",
                        "Use when the user asks for arithmetic or numeric calculation.",
                    ),
                    root.join("accurate-calculation"),
                    include_str!("../skills/accurate-calculation/SKILL.md"),
                ),
                SkillDocument::new(
                    SkillSummary::new(
                        "current-time",
                        "Use when the user asks for the current date, time, or timezone-aware now.",
                    ),
                    root.join("current-time"),
                    include_str!("../skills/current-time/SKILL.md"),
                ),
            ],
        }
    }
}

#[async_trait]
impl SkillStore for ExampleSkillStore {
    async fn list(&self) -> Result<Vec<SkillSummary>> {
        Ok(self
            .skills
            .iter()
            .map(|skill| skill.summary.clone())
            .collect())
    }

    async fn load(&self, name: &str) -> Result<SkillDocument> {
        self.skills
            .iter()
            .find(|skill| skill.summary.name == name)
            .cloned()
            .ok_or_else(|| Error::client(format!("unknown skill: {name}")))
    }
}
