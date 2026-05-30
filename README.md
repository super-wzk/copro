# Copro

Copro is a Rust workspace for building controllable agent runtimes. It provides a step-level agent core, model provider adapters, local tool routing, skills, and workspace tools.

## Crates

- `copro-agent`: schedulable agent runtime, turn handles, checkpoints, and control APIs.
- `copro-api`: shared message, request, response, stream, and tool types.
- `copro-provider-openai`: OpenAI Responses API provider implementation.
- `copro-harness`: local tools and skills helpers for examples and applications.
- `copro-workspace`: workspace-oriented tool router.

## Quick Start

Run the included CLI example:

```sh
export OPENAI_API_KEY="sk-..."
cargo run -p simple-cli
```

Or start with a minimal no-tools agent:

```rust
use copro_agent::{AgentEvent, AgentHistory, AgentTurnConfig, InputMessage, start_turn};
use copro_api::message::InputContent;
use copro_harness::tools::LocalToolRouter;
use copro_provider_openai::{
    OpenAiResponsesModelConfig, OpenAiResponsesProvider, OpenAiResponsesProviderConfig,
};
use futures_util::StreamExt;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let provider = OpenAiResponsesProvider::new(OpenAiResponsesProviderConfig {
        api_key: std::env::var("OPENAI_API_KEY").ok(),
        ..OpenAiResponsesProviderConfig::default()
    });
    let model = provider.model("gpt-5.5", OpenAiResponsesModelConfig::default())?;

    let mut history = AgentHistory::default();
    history.push_input(InputMessage::User(vec![InputContent::Text(
        "Write one sentence about Rust agents.".to_string(),
    )]));

    let turn = start_turn(
        history,
        AgentTurnConfig::default(),
        model,
        Arc::new(LocalToolRouter::default()),
    );
    let history_after_turn = turn.clone();
    let mut stream = turn.events();
    while let Some(event) = stream.next().await {
        if let AgentEvent::ModelDelta { delta, .. } = event? {
            print_delta(delta);
        }
    }
    let _history = history_after_turn.into_history().await;

    Ok(())
}

fn print_delta(delta: copro_api::stream::OutputContentDelta) {
    match delta {
        copro_api::stream::OutputContentDelta::Text(text) => print!("{text}"),
        copro_api::stream::OutputContentDelta::Thinking(text) => eprint!("{text}"),
        copro_api::stream::OutputContentDelta::Image(_) => {}
        copro_api::stream::OutputContentDelta::ToolCall { .. } => {}
    }
}
```

Every execution starts with `start_turn(history, config, model, tools)`. The application owns `AgentHistory` between turns; each turn consumes a history value and `AgentTurnHandle::into_history().await` returns the updated history after the turn completes. Use `AgentTurnHandle::events()` for automatic streaming, or drive `AgentTurnHandle::step_until_control()` manually, inspect `point.checkpoint()`, and resume with `point.continue_turn().await` or `point.control(...)`.

## Pause And Resume

Application code pauses an agent turn through `AgentTurnHandle`. Keep a cloned handle in your UI, HTTP handler, or scheduler, then call `pause()` and `resume()` from that layer.

```rust
use copro_agent::{AgentEvent, AgentTurnHandle};
use futures_util::StreamExt;

async fn drive_turn(turn: AgentTurnHandle) -> copro_api::error::Result<()> {
    let controls = turn.clone();

    tokio::spawn(async move {
        // Call this from your application event, such as a pause button.
        controls.pause().await.ok();

        // Call this later from a resume button or scheduler decision.
        controls.resume().await.ok();
    });

    let mut events = turn.events();
    while let Some(event) = events.next().await {
        match event? {
            AgentEvent::TurnPaused { .. } => {
                // Update application state: the turn is paused.
            }
            AgentEvent::TurnResumed { .. } => {
                // Update application state: the turn resumed.
            }
            event => {
                // Handle normal stream, tool, and checkpoint events.
                let _ = event;
            }
        }
    }

    Ok(())
}
```

If your application already drives checkpoints manually, pause at the current checkpoint:

```rust
use copro_agent::{AgentControl, AgentTurnHandle};

async fn pause_at_checkpoint(turn: &AgentTurnHandle) -> copro_api::error::Result<()> {
    let point = turn.step_until_control().await?;
    point.control(AgentControl::Pause).await?;

    // Later, after user or scheduler approval:
    turn.resume().await?;

    Ok(())
}
```

`pause()` does not cancel the in-flight model stream or tool call. It requests suspension at the next control boundary. Use `preempt()` when the application needs to interrupt the current in-flight action.

## Development

```sh
cargo fmt
cargo test
cargo clippy --workspace --all-targets -- -D warnings
```
