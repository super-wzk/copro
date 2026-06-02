use super::{SkillDocument, SkillSummary};
use copro_api::async_trait;
use copro_api::error::Result;

/// Source of already-discovered and already-parsed skills.
///
/// This runtime intentionally does not scan directories or parse `SKILL.md`.
/// Callers provide an implementation backed by files, configuration, memory, or
/// any other source.
#[async_trait]
pub trait SkillStore: Send + Sync {
    async fn list(&self) -> Result<Vec<SkillSummary>>;
    async fn load(&self, name: &str) -> Result<SkillDocument>;
}
