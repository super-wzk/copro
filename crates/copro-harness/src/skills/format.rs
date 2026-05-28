use super::{SkillDocument, SkillSummary};

pub(crate) fn format_available_skills_prompt(summaries: &[SkillSummary]) -> Option<String> {
    if summaries.is_empty() {
        return None;
    }

    let skills = summaries
        .iter()
        .map(|skill| format!("- `{}`: {}", skill.name, skill.description))
        .collect::<Vec<_>>()
        .join("\n");

    Some(format!(
        "\
Use skills via progressive disclosure. If the task matches a skill below, call `load_skill` once with the exact skill name, then follow the loaded instructions. Run skill scripts/assets only when those instructions require it.

## Available Skills

{skills}
"
    ))
}

pub(crate) fn format_skill_document(skill: &SkillDocument) -> String {
    let content = skill.content.trim_end_matches('\n');
    let name = &skill.summary.name;
    let description = &skill.summary.description;
    let root = skill.root.display();

    format!(
        "\
## Loaded Skill

- Name: `{name}`
- Description: {description}
- Root: `{root}` — resolve relative file references against this directory.

--- SKILL.md ---
{content}
--- END SKILL.md ---"
    )
}
