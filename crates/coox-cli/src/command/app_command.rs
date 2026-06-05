#[derive(Debug, Clone, PartialEq)]
pub enum AppCommand {
    Ui(UiCommand),
    Runtime(RuntimeCommand),
}

#[derive(Debug, Clone, PartialEq)]
pub enum UiCommand {
    ShowHelp,
    ClearConversation,
    PushCommandOutput(String),
    Scroll { rows: i32 },
    ScrollToBottom,
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeCommand {
    ClearSessionHistory,
    SwitchModel(String),
    StopTurn,
    PauseTurn,
    ResumeTurn,
}
