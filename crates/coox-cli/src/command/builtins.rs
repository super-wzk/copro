use crate::command::app_command::{AppCommand, RuntimeCommand, UiCommand};
use crate::command::slash::{
    SessionSnapshot, SlashCommand, SlashCommandRegistry, SlashCommandSpec, SlashError,
};

pub static BUILTIN_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        spec: SlashCommandSpec {
            name: "help",
            summary: "Show local slash commands",
            usage: "/help",
        },
        build: build_help,
    },
    SlashCommand {
        spec: SlashCommandSpec {
            name: "clear",
            summary: "Clear session history",
            usage: "/clear",
        },
        build: build_clear,
    },
    SlashCommand {
        spec: SlashCommandSpec {
            name: "quit",
            summary: "Quit coox-cli",
            usage: "/quit",
        },
        build: build_quit,
    },
    SlashCommand {
        spec: SlashCommandSpec {
            name: "model",
            summary: "Show or switch model",
            usage: "/model [id]",
        },
        build: build_model,
    },
    SlashCommand {
        spec: SlashCommandSpec {
            name: "stop",
            summary: "Abort the active turn",
            usage: "/stop",
        },
        build: build_stop,
    },
    SlashCommand {
        spec: SlashCommandSpec {
            name: "pause",
            summary: "Pause the active turn",
            usage: "/pause",
        },
        build: build_pause,
    },
    SlashCommand {
        spec: SlashCommandSpec {
            name: "resume",
            summary: "Resume the active turn",
            usage: "/resume",
        },
        build: build_resume,
    },
];

pub fn builtins() -> SlashCommandRegistry {
    SlashCommandRegistry::new(BUILTIN_COMMANDS)
}

fn build_help(args: &str, _snapshot: &SessionSnapshot<'_>) -> Result<Vec<AppCommand>, SlashError> {
    no_args(args, "/help")?;
    Ok(vec![AppCommand::Ui(UiCommand::ShowHelp)])
}

fn build_clear(args: &str, _snapshot: &SessionSnapshot<'_>) -> Result<Vec<AppCommand>, SlashError> {
    no_args(args, "/clear")?;
    Ok(vec![
        AppCommand::Runtime(RuntimeCommand::ClearSessionHistory),
        AppCommand::Ui(UiCommand::ClearConversation),
    ])
}

fn build_quit(args: &str, _snapshot: &SessionSnapshot<'_>) -> Result<Vec<AppCommand>, SlashError> {
    no_args(args, "/quit")?;
    Ok(vec![AppCommand::Ui(UiCommand::Quit)])
}

fn build_model(args: &str, snapshot: &SessionSnapshot<'_>) -> Result<Vec<AppCommand>, SlashError> {
    let args = args.trim();
    if args.is_empty() {
        return Ok(vec![AppCommand::Ui(UiCommand::PushCommandOutput(format!(
            "model: {}",
            snapshot.model_id
        )))]);
    }

    one_arg(args, "/model [id]").map(|model| {
        vec![AppCommand::Runtime(RuntimeCommand::SwitchModel(
            model.to_string(),
        ))]
    })
}

fn build_stop(args: &str, _snapshot: &SessionSnapshot<'_>) -> Result<Vec<AppCommand>, SlashError> {
    no_args(args, "/stop")?;
    Ok(vec![AppCommand::Runtime(RuntimeCommand::StopTurn)])
}

fn build_pause(args: &str, _snapshot: &SessionSnapshot<'_>) -> Result<Vec<AppCommand>, SlashError> {
    no_args(args, "/pause")?;
    Ok(vec![AppCommand::Runtime(RuntimeCommand::PauseTurn)])
}

fn build_resume(
    args: &str,
    _snapshot: &SessionSnapshot<'_>,
) -> Result<Vec<AppCommand>, SlashError> {
    no_args(args, "/resume")?;
    Ok(vec![AppCommand::Runtime(RuntimeCommand::ResumeTurn)])
}

fn no_args(args: &str, usage: &'static str) -> Result<(), SlashError> {
    if args.trim().is_empty() {
        Ok(())
    } else {
        Err(SlashError::Usage {
            message: "unexpected arguments".to_string(),
            usage,
        })
    }
}

fn one_arg<'a>(args: &'a str, usage: &'static str) -> Result<&'a str, SlashError> {
    let mut parts = args.split_whitespace();
    let Some(value) = parts.next() else {
        return Err(SlashError::Usage {
            message: "missing argument".to_string(),
            usage,
        });
    };
    if parts.next().is_some() {
        return Err(SlashError::Usage {
            message: "too many arguments".to_string(),
            usage,
        });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::slash::TurnSnapshot;

    fn snapshot() -> SessionSnapshot<'static> {
        SessionSnapshot {
            model_id: "gpt-test",
            turn_state: TurnSnapshot::Idle,
        }
    }

    #[test]
    fn help_registry_includes_all_builtin_commands() {
        let registry = builtins();
        let names = registry
            .iter()
            .map(|command| command.spec.name)
            .collect::<Vec<_>>();

        for expected in ["help", "clear", "quit", "model", "stop", "pause", "resume"] {
            assert!(names.contains(&expected), "{expected} missing");
        }

        for removed in ["reset", "fold", "temp", "toolchoice"] {
            assert!(!names.contains(&removed), "{removed} should be removed");
        }
    }

    #[test]
    fn clear_resets_runtime_and_visible_conversation() {
        let registry = builtins();
        let command = registry.find("clear").expect("clear command");

        let commands = (command.build)("", &snapshot()).expect("clear command");

        assert_eq!(
            commands,
            vec![
                AppCommand::Runtime(RuntimeCommand::ClearSessionHistory),
                AppCommand::Ui(UiCommand::ClearConversation),
            ]
        );
    }

    #[test]
    fn model_without_args_outputs_current_model() {
        let registry = builtins();
        let command = registry.find("model").expect("model command");

        let commands = (command.build)("", &snapshot()).expect("model output");

        assert_eq!(
            commands,
            vec![AppCommand::Ui(UiCommand::PushCommandOutput(
                "model: gpt-test".to_string()
            ))]
        );
    }
}
