use crate::command::app_command::AppCommand;

pub type SlashCommandBuildFn =
    for<'a> fn(&str, &SessionSnapshot<'a>) -> Result<Vec<AppCommand>, SlashError>;

#[derive(Debug, Clone, Copy)]
pub struct SlashCommandSpec {
    pub name: &'static str,
    pub summary: &'static str,
    pub usage: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub struct SlashCommand {
    pub spec: SlashCommandSpec,
    pub build: SlashCommandBuildFn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashError {
    Usage {
        message: String,
        usage: &'static str,
    },
    Message(String),
}

impl SlashError {
    pub fn render(&self) -> String {
        match self {
            Self::Usage { message, usage } => format!("{message}\nusage: {usage}"),
            Self::Message(message) => message.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SlashCommandRegistry {
    commands: &'static [SlashCommand],
}

impl SlashCommandRegistry {
    pub const fn new(commands: &'static [SlashCommand]) -> Self {
        Self { commands }
    }

    pub fn find(&self, name: &str) -> Option<&SlashCommand> {
        self.commands
            .iter()
            .find(|command| command.spec.name == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &'static SlashCommand> + '_ {
        self.commands.iter()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionSnapshot<'a> {
    pub model_id: &'a str,
    pub turn_state: TurnSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnSnapshot {
    Idle,
    Running,
    Paused,
    PendingAck,
    Failed,
}

#[cfg(test)]
mod tests {
    use crate::command::builtins::builtins;

    #[test]
    fn registry_finds_command_names() {
        let registry = builtins();

        let command = registry.find("quit").expect("quit command");

        assert_eq!(command.spec.name, "quit");
        assert!(registry.find("q").is_none());
    }
}
