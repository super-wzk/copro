#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputIntent {
    UserText(String),
    Slash(SlashInvocation),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashInvocation {
    pub name: String,
    pub args: String,
}

pub fn parse_input(text: &str) -> Option<InputIntent> {
    if text.trim().is_empty() {
        return None;
    }

    if let Some(rest) = text.strip_prefix("//") {
        return Some(InputIntent::UserText(format!("/{rest}")));
    }

    let Some(rest) = text.strip_prefix('/') else {
        return Some(InputIntent::UserText(text.to_string()));
    };

    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default().to_string();
    let args = parts
        .next()
        .map(str::trim_start)
        .unwrap_or_default()
        .to_string();

    Some(InputIntent::Slash(SlashInvocation { name, args }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_only_does_not_submit() {
        assert_eq!(parse_input(" \n\t "), None);
    }

    #[test]
    fn normal_text_stays_user_text() {
        assert_eq!(
            parse_input("hello /help"),
            Some(InputIntent::UserText("hello /help".to_string()))
        );
    }

    #[test]
    fn double_slash_escapes_user_text() {
        assert_eq!(
            parse_input("//hello"),
            Some(InputIntent::UserText("/hello".to_string()))
        );
    }

    #[test]
    fn slash_command_splits_name_and_args() {
        assert_eq!(
            parse_input("/model   gpt-5.5"),
            Some(InputIntent::Slash(SlashInvocation {
                name: "model".to_string(),
                args: "gpt-5.5".to_string(),
            }))
        );
    }
}
