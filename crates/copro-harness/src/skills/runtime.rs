use super::format::format_available_skills_prompt;
use super::{SkillDocument, SkillStore};
use copro_api::error::{Error, Result};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

/// Runtime state for progressive skill disclosure.
pub struct SkillRuntime {
    store: Arc<dyn SkillStore>,
    loaded: RwLock<BTreeMap<String, SkillDocument>>,
}

impl SkillRuntime {
    pub fn new(store: Arc<dyn SkillStore>) -> Self {
        Self {
            store,
            loaded: RwLock::new(BTreeMap::new()),
        }
    }

    pub(crate) async fn load(&self, name: &str) -> Result<SkillDocument> {
        if let Some(skill) = self.cached(name)? {
            return Ok(skill);
        }

        let skill = self.store.load(name).await?;
        if skill.summary.name != name {
            return Err(Error::client(format!(
                "skill store returned `{}` while loading `{name}`",
                skill.summary.name
            )));
        }

        self.loaded_write()?.insert(name.to_string(), skill.clone());
        Ok(skill)
    }

    pub(crate) async fn available_skills_prompt(&self) -> Result<Option<String>> {
        let mut summaries = self.store.list().await?;
        summaries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(format_available_skills_prompt(&summaries))
    }

    fn cached(&self, name: &str) -> Result<Option<SkillDocument>> {
        Ok(self.loaded_read()?.get(name).cloned())
    }

    fn loaded_read(
        &self,
    ) -> Result<std::sync::RwLockReadGuard<'_, BTreeMap<String, SkillDocument>>> {
        self.loaded
            .read()
            .map_err(|_| Error::client("skill runtime cache lock poisoned"))
    }

    fn loaded_write(
        &self,
    ) -> Result<std::sync::RwLockWriteGuard<'_, BTreeMap<String, SkillDocument>>> {
        self.loaded
            .write()
            .map_err(|_| Error::client("skill runtime cache lock poisoned"))
    }
}
