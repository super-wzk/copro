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
Skills are available via progressive disclosure. When the user's task matches a skill description, call `load_skill` with the exact skill name before following that skill. If the full skill instructions already appear in the conversation, use them instead of loading the same skill again. Skill scripts and assets are not executed automatically; use normal tools only when the loaded instructions say to do so.

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
Skill loaded. Follow these instructions for the current task.

## Skill

- Name: `{name}`
- Description: {description}
- Root: `{root}`

Resolve relative file references in this skill against the root above. Do not auto-run scripts; use normal tools only when the instructions require it.

--- SKILL.md ---
{content}
--- END SKILL.md ---"
    )
}
