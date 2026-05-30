use copro_agent::{CancellationToken, ToolRouter};
use copro_api::async_trait;
use copro_api::error::{Error, Result};
use copro_api::message::{
    InputContent, InputMessage, Message, OutputContent, OutputMessage, ToolCall, ToolResult,
    ToolResultStatus,
};
use copro_api::request::{GenerateRequest, GenerateRequestOptions};
use copro_harness::skills::{
    SkillDocument, SkillRequestInjector, SkillRuntime, SkillStore, SkillSummary, SkillToolRouter,
};
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn request_injector_adds_available_skills_after_initial_instructions() {
    let runtime = runtime_with(vec![skill("test-skill", "Use for testing.")]);
    let injector = SkillRequestInjector::new(runtime);
    let mut request = GenerateRequest {
        messages: vec![
            Message::system(vec![InputContent::Text("system".to_string())]),
            Message::user(vec![InputContent::Text("hi".to_string())]),
        ],
        tools: Vec::new(),
        tool_choice: None,
        hosted_tools: Vec::new(),
        options: GenerateRequestOptions::default(),
    };

    injector.prepare_request(&mut request).await.unwrap();

    assert!(matches!(
        request.messages.first(),
        Some(Message::Input(InputMessage::System(_)))
    ));
    assert!(matches!(
        request.messages.get(1),
        Some(Message::Input(InputMessage::Developer(_)))
    ));
    assert!(matches!(
        request.messages.get(2),
        Some(Message::Input(InputMessage::User(_)))
    ));
}

#[tokio::test]
async fn request_injector_prunes_loaded_skill_context_from_previous_turns() {
    let runtime = runtime_with(vec![skill("test-skill", "Use for testing.")]);
    let injector = SkillRequestInjector::new(runtime);
    let mut request = GenerateRequest {
        messages: vec![
            Message::system(vec![InputContent::Text("system".to_string())]),
            Message::user(vec![InputContent::Text("old request".to_string())]),
            Message::assistant(vec![OutputContent::ToolCall(load_skill_call(
                "load_skill",
                "test-skill",
            ))]),
            load_skill_result(),
            Message::assistant(vec![OutputContent::Text("done".to_string())]),
            Message::user(vec![InputContent::Text("new request".to_string())]),
        ],
        tools: Vec::new(),
        tool_choice: None,
        hosted_tools: Vec::new(),
        options: GenerateRequestOptions::default(),
    };

    injector.prepare_request(&mut request).await.unwrap();

    assert_eq!(request.messages.len(), 5);
    assert!(matches!(
        request.messages.first(),
        Some(Message::Input(InputMessage::System(_)))
    ));
    assert!(matches!(
        request.messages.get(1),
        Some(Message::Input(InputMessage::Developer(_)))
    ));
    assert!(matches!(
        request.messages.get(2),
        Some(Message::Input(InputMessage::User(_)))
    ));
    assert!(matches!(
        request.messages.get(3),
        Some(Message::Output(OutputMessage::Assistant(content)))
            if matches!(content.first(), Some(OutputContent::Text(text)) if text == "done")
    ));
    assert!(matches!(
        request.messages.get(4),
        Some(Message::Input(InputMessage::User(_)))
    ));
    assert!(!has_load_skill_tool_call(&request.messages));
    assert!(!has_load_skill_result(&request.messages));
}

#[tokio::test]
async fn request_injector_keeps_loaded_skill_context_in_current_turn() {
    let runtime = runtime_with(vec![skill("test-skill", "Use for testing.")]);
    let injector = SkillRequestInjector::new(runtime);
    let mut request = GenerateRequest {
        messages: vec![
            Message::system(vec![InputContent::Text("system".to_string())]),
            Message::user(vec![InputContent::Text("current request".to_string())]),
            Message::assistant(vec![OutputContent::ToolCall(load_skill_call(
                "load_skill",
                "test-skill",
            ))]),
            load_skill_result(),
        ],
        tools: Vec::new(),
        tool_choice: None,
        hosted_tools: Vec::new(),
        options: GenerateRequestOptions::default(),
    };

    injector.prepare_request(&mut request).await.unwrap();

    assert!(has_load_skill_tool_call(&request.messages));
    assert!(has_load_skill_result(&request.messages));
}

#[tokio::test]
async fn skill_tool_router_exposes_and_executes_load_skill() {
    let runtime = runtime_with(vec![skill("test-skill", "Use for testing.")]);
    let router = SkillToolRouter::new(runtime);

    let definitions = router.definitions().await.unwrap();
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].name, "load_skill");

    let result = router
        .execute(
            load_skill_call(&definitions[0].name, "test-skill"),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Success);
    assert_eq!(result.name, "load_skill");
    assert!(tool_text(&result).contains("# test-skill"));
    assert!(tool_text(&result).contains("/skills/test-skill"));
}

#[tokio::test]
async fn skill_tool_router_rejects_unknown_skill_tools() {
    let runtime = runtime_with(vec![skill("test-skill", "Use for testing.")]);
    let router = SkillToolRouter::new(runtime);

    let result = router
        .execute(
            ToolCall {
                id: "call-2".into(),
                name: "echo".to_string(),
                arguments: serde_json::Map::new(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(result.status, ToolResultStatus::Error);
    assert_eq!(tool_text(&result), "unknown skill tool: echo");
}

#[tokio::test]
async fn load_skill_reads_from_store_each_time() {
    let store = Arc::new(CountingStore {
        loads: AtomicUsize::new(0),
        skill: skill("dynamic-skill", "Use current file contents."),
    });
    let router = SkillToolRouter::new(Arc::new(SkillRuntime::new(store.clone())));
    let tool_name = router.definitions().await.unwrap()[0].name.clone();

    router
        .execute(
            load_skill_call(&tool_name, "dynamic-skill"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    router
        .execute(
            load_skill_call(&tool_name, "dynamic-skill"),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(store.loads.load(Ordering::SeqCst), 2);
}

fn runtime_with(skills: Vec<SkillDocument>) -> Arc<SkillRuntime> {
    Arc::new(SkillRuntime::new(Arc::new(TestStore { skills })))
}

fn skill(name: &str, description: &str) -> SkillDocument {
    SkillDocument::new(
        SkillSummary::new(name, description),
        format!("/skills/{name}"),
        format!("# {name}\n\nInstructions."),
    )
}

fn load_skill_call(tool_name: &str, skill_name: &str) -> ToolCall {
    ToolCall {
        id: "call-1".into(),
        name: tool_name.to_string(),
        arguments: serde_json::Map::from_iter([(
            "name".to_string(),
            Value::String(skill_name.to_string()),
        )]),
    }
}

fn load_skill_result() -> Message {
    Message::tool(ToolResult {
        call_id: "call-1".into(),
        name: "load_skill".to_string(),
        status: ToolResultStatus::Success,
        content: vec![InputContent::Text("full skill".to_string())],
    })
}

fn has_load_skill_tool_call(messages: &[Message]) -> bool {
    messages.iter().any(|message| match message {
        Message::Output(OutputMessage::Assistant(content)) => content.iter().any(|item| {
            matches!(
                item,
                OutputContent::ToolCall(tool_call) if tool_call.name == "load_skill"
            )
        }),
        _ => false,
    })
}

fn has_load_skill_result(messages: &[Message]) -> bool {
    messages.iter().any(|message| {
        matches!(
            message,
            Message::Output(OutputMessage::Tool(result)) if result.name == "load_skill"
        )
    })
}

fn tool_text(result: &ToolResult) -> &str {
    let Some(InputContent::Text(text)) = result.content.first() else {
        panic!("expected text tool result");
    };
    text
}

struct TestStore {
    skills: Vec<SkillDocument>,
}

#[async_trait]
impl SkillStore for TestStore {
    async fn list(&self) -> Result<Vec<SkillSummary>> {
        Ok(self
            .skills
            .iter()
            .map(|skill| skill.summary.clone())
            .collect())
    }

    async fn load(&self, name: &str) -> Result<SkillDocument> {
        self.skills
            .iter()
            .find(|skill| skill.summary.name == name)
            .cloned()
            .ok_or_else(|| Error::client(format!("unknown skill: {name}")))
    }
}

struct CountingStore {
    loads: AtomicUsize,
    skill: SkillDocument,
}

#[async_trait]
impl SkillStore for CountingStore {
    async fn list(&self) -> Result<Vec<SkillSummary>> {
        Ok(vec![self.skill.summary.clone()])
    }

    async fn load(&self, name: &str) -> Result<SkillDocument> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        if name == self.skill.summary.name {
            Ok(self.skill.clone())
        } else {
            Err(Error::client(format!("unknown skill: {name}")))
        }
    }
}
