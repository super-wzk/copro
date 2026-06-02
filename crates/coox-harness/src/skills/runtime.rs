use super::format::format_available_skills_prompt;
use super::{SkillDocument, SkillStore};
use copro_api::error::{Error, Result};
use std::sync::Arc;

/// Runtime state for progressive skill disclosure.
pub struct SkillRuntime {
    store: Arc<dyn SkillStore>,
}

impl SkillRuntime {
    pub fn new(store: Arc<dyn SkillStore>) -> Self {
        Self { store }
    }

    pub(crate) async fn load(&self, name: &str) -> Result<SkillDocument> {
        let skill = self.store.load(name).await?;
        if skill.summary.name != name {
            return Err(Error::client(format!(
                "skill store returned `{}` while loading `{name}`",
                skill.summary.name
            )));
        }

        Ok(skill)
    }

    pub(crate) async fn available_skills_prompt(&self) -> Result<Option<String>> {
        let mut summaries = self.store.list().await?;
        summaries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(format_available_skills_prompt(&summaries))
    }
}
