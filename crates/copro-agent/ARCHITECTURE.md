# copro-agent Architecture Plan

## 目标

`copro-agent` 是一个通用 Agent framework。核心执行模型需要从“按粗粒度事件推进”改为“按 agent step clock 推进”。

新的底层设计目标：

- `AgentStep` 是最小时间单位，类似 CPU clock tick。
- 每个 step 执行一条精简、明确的 `AgentAction`。
- 外部系统可以在 step boundary 观察、暂停、恢复、抢占、改写或终止 run。
- 支持后续多 agent 调度、人工介入、UI 控制、trace/replay，但不把这些场景固化进底层框架。
- 保持 agentic 命名，避免用 actor、scheduler、stepper、effect 这类偏技术或语义混淆的核心领域名。

这是破坏性架构设计。现有 `AgentActor`、`AgentState`、`AgentScheduler`、`TurnStepper`、传统 hook 以及旧粗粒度 `AgentEvent` 都不再作为核心领域概念长期保留。

## 设计原则

- Agent framework 的核心术语必须围绕 agent execution，而不是 runtime 实现细节。
- 底层只提供通用执行协议，不内置 coding agent、research agent、planner agent 等上层业务形态。
- agent 的业务判断和外部运行控制分离。业务判断属于 `AgentPolicy` / `AgentPlanner` 等上层扩展；底层运行控制使用 `AgentControl`。
- 长期状态、一次执行、单 turn 状态、单 step clock 必须分层清楚。
- async IO 只发生在执行层；纯状态推进不能持有 stream、task handle、cancellation token。
- history commit 只能在明确的 step 中完成，且 commit 后才发出 committed event。
- 兼容 API 只能作为 adapter，不能反向决定核心协议。

## 核心领域模型

公开入口和核心执行概念：

```text
Agent          // public façade，创建 context/run，提供高层 API；不是执行核心
AgentContext   // 长期 agent 上下文，拥有 history/config/model/tools
AgentRun       // 一次可调度执行实例，拥有 lifecycle/in-flight/clock
AgentRunHandle // 外部调度器控制 AgentRun 的 public handle
AgentTurn      // 单 turn 纯状态机，根据 outcome 选择下一条 action
AgentStep      // 最小 clock tick，一次只执行一个 action
AgentAction    // RISC-like 原子指令
AgentOutcome   // action 执行后的结果
AgentControl   // 外部对当前 step boundary 的运行控制
AgentEvent     // 对外观察事件，不承载控制语义
```

不作为长期核心概念保留：

```text
AgentActor      // 技术实现名，删除
AgentState      // 拆为 AgentContext + AgentRun 内部状态
AgentScheduler  // 技术实现名，删除
TurnStepper     // 改为 AgentTurn
TurnEffect      // 改为 AgentAction
旧 step 干预载荷 // 改为 AgentControl，避免和 agent 业务判断混淆
AgentHook       // 不继续扩展，迁移到 step boundary control/observer
```

## 模块命名空间

public API 保持 crate root 扁平 re-export，避免把外部用户绑定到内部文件组织。

当前内部模块布局：

```text
src/agent.rs          // public Agent façade
src/context.rs        // long-lived context + command loop
src/event.rs          // AgentEvent / AgentStream
src/tools.rs          // ToolRouter / ToolExecutionPolicy
src/turn.rs           // pure turn state machine
src/cancel.rs         // per-run cancellation internals

src/run/mod.rs        // run namespace declarations + re-export only
src/run/types.rs      // ids, AgentStep, AgentAction, AgentOutcome, AgentRunState
src/run/control.rs    // AgentControl, AgentControlKind, AgentControlSignal
src/run/checkpoint.rs // AgentStepReport, AgentCheckpoint
src/run/handle.rs     // AgentRunHandle public control surface
src/run/execution.rs  // internal AgentRun execution loop
```

`runtime` 不作为 public namespace 暴露；`StopSignal` public API 已移除。in-flight cancellation 是每个 run 的内部机制，由 `AgentRunHandle` 的 `Abort*` / `preempt()` 触发。

## 分层边界

### Agent

`Agent` 是公开 handle，保持轻量、可 clone。

职责：

- 创建和访问 `AgentContext`。
- 启动 `AgentRun`。
- 暴露高层 API，例如 `run_stream()`。
- 暴露低层可调度 API，例如 `start_run()`。
- 不拥有 in-flight model stream 或 tool task。
- 不直接修改 history。
- 不承载 turn 状态机逻辑。

示例 API：

```rust
impl Agent {
    pub fn new(model: Arc<dyn Model>, tools: Arc<dyn ToolRouter>) -> Self;

    pub async fn start_run(&self) -> Result<AgentRunHandle>;
    pub fn run_stream(&self) -> AgentStream;

    pub async fn push_message(&self, message: Message) -> Result<()>;
    pub async fn messages(&self) -> Result<Vec<Message>>;
    pub async fn replace_messages(&self, messages: Vec<Message>) -> Result<()>;
}
```

### AgentContext

`AgentContext` 是 agent 的长期上下文，不是执行器。

职责：

- 拥有 conversation history：`Vec<Message>`。
- 拥有长期配置：`model`、`tools`、`options`、`tool_choice`、`hosted_tools`。
- 提供 history/config 的受控读写入口。
- 约束同一 context 的 active mutable run。

不负责：

- 不执行 `AgentAction`。
- 不持有 `ModelStream`。
- 不持有 tool task handle。
- 不处理 pause/resume/preempt。
- 不维护 `AgentTurn`。

并发边界：第一版同一个 `AgentContext` 只允许一个 active mutable `AgentRun`。多 agent 调度通过多个 context/run 组合实现。未来如需并发分支，应该引入显式 `fork` / snapshot，而不是让多个 run 隐式共享写入同一份 history。

### AgentRun

`AgentRun` 是一次可调度执行实例，是底层控制的核心。

职责：

- 拥有 run lifecycle。
- 拥有当前 `AgentTurn`。
- 拥有 step clock。
- 执行 `AgentAction`。
- 持有 in-flight model stream、tool task、cancellation token。
- 在 step boundary 应用 `AgentControl`。
- 通过受控接口向 `AgentContext` commit history。
- 产出 `AgentEvent`。
- 执行 recovery，保证 history 合法。

不负责：

- 不做 agent 业务规划。
- 不把 coding agent、多 agent、人工审批等场景写死进底层。
- 不允许外部绕过 step boundary 直接改写 run 内部状态。
- 不内置可替换 scheduler 策略；外部调度器通过 `AgentRunHandle` 驱动。

生命周期建议：

```rust
pub enum AgentRunState {
    Ready { next: AgentAction, step_id: AgentStepId },
    InFlight { step: AgentStep },
    WaitingControl { step: AgentStep, outcome: AgentOutcome },
    Paused { at: AgentStepId },
    Preempting { step_id: AgentStepId },
    Recovering { after: AgentStepId },
    Finished,
    Aborted,
}
```

`Ready` 由 `StepReady` 事件驱动，`Recovering` 由 `RunRecovering` 事件驱动。recovery 期间产生的 tool result commit 仍走普通 `AgentStep` clock。

### AgentRunHandle

`AgentRunHandle` 是外部调度器、UI、debugger、safety layer 控制 `AgentRun` 的公开句柄。

职责：

- 以 command 形式驱动一个 active `AgentRun`。
- 暴露单步推进、运行到 control point、应用 control、pause/resume/preempt、读取 state。
- 保证所有外部控制都经过当前 `AgentStepId` 校验。
- 保证外部调度器只能在 step boundary 控制 run，不能直接改 `AgentTurn` 或 context history。
- 可以被不同上层调度策略复用。

不负责：

- 不决定 agent 业务规划。
- 不内置多 agent orchestration。
- 不定义 scheduler plugin trait 作为核心领域模型。
- 不允许多个 active mutable run 隐式共享同一个 `AgentContext` 写 history。

建议 API：

```rust
impl Agent {
    pub async fn start_run(&self) -> Result<AgentRunHandle>;
}

impl AgentRunHandle {
    pub async fn step(&self) -> Result<AgentStepReport>;
    pub async fn step_until_control(&self) -> Result<AgentCheckpoint>;
    pub async fn control(&self, step_id: AgentStepId, control: AgentControl) -> Result<AgentStepReport>;
    pub async fn pause(&self) -> Result<()>;
    pub async fn resume(&self) -> Result<()>;
    pub async fn preempt(&self) -> Result<()>;
    pub async fn state(&self) -> Result<AgentRunState>;
    pub fn events(self) -> AgentStream;
}

pub struct AgentStepReport {
    pub step: AgentStep,
    pub outcome: AgentOutcome,
    pub state: AgentRunState,
    pub events: Vec<AgentEvent>,
}

pub enum AgentCheckpoint {
    Basic(AgentStepReport),
    RequestBuilt(AgentStepReport),
    ModelDelta(AgentStepReport),
    AssistantOutput(AgentStepReport),
    ToolPlanned(AgentStepReport),
    ToolRejected(AgentStepReport),
    ToolResult(AgentStepReport),
}
```

### AgentTurn

`AgentTurn` 是单 turn 的纯状态机。

当前实现保持同步、无 IO、无 cancellation token；完整 `next_action()` / `apply_outcome()` 拆分属于后续机械重构，不和本轮 control 语义迁移混在一起。

职责：

- 维护 turn 内部 pending 数据，例如 tools、request、model output、tool calls、tool results。
- 根据上一个 `AgentOutcome` 计算下一条 `AgentAction`。
- 将下一步所需数据作为 `AgentAction` operands 显式返回，避免执行层依赖隐式 pending lookup。
- 判断 turn 是否完成。

严格限制：

- 不 async。
- 不持有 `ModelStream`。
- 不持有 tool task handle。
- 不持有 cancellation token。
- 不直接调用 model/tools。
- 不直接 push `Message`。
- 不知道 pause/resume/preempt。
- 不知道多 agent 调度。
- 不产出旧兼容事件。

建议 API：

```rust
impl AgentTurn {
    pub fn next_action(&self) -> Result<AgentAction>;
    pub fn apply_outcome(&mut self, outcome: AgentOutcome) -> Result<()>;
    pub fn is_finished(&self) -> bool;
}
```

### AgentStep

`AgentStep` 是执行 clock 的最小 tick。

职责：

- 绑定一个稳定 `AgentStepId`。
- 绑定一条 `AgentAction`。
- 执行开始后进入 in-flight。
- action 产生 outcome 后进入 step boundary。
- boundary 处应用 `AgentControl`。
- commit 后 clock 前进。

建议结构：

```rust
pub struct AgentStepId {
    pub run_id: AgentRunId,
    pub turn_id: AgentTurnId,
    pub tick: u64,
}

pub struct AgentStep {
    pub id: AgentStepId,
    pub action: AgentAction,
}
```

clock 规则：

- `tick` 在同一个 `AgentRun` 内单调递增。
- 一个 step 未完成前不能启动下一个 step。
- step outcome commit 后 `tick += 1`。
- `Pause` 不创建新的 hidden step；它在当前 boundary 生效，使 run 停在下一步之前。
- 如果 pause request 发生在 in-flight action 期间，当前 action 不会被取消；run 会在下一次 boundary 发出 `RunPaused`，直到收到 resume 后发出 `RunResumed` 并继续。
- `AbortTurn` 结束当前 turn/run，`AbortRun` 直接产生 run aborted。
- `Preempt` 可以中断 in-flight step，并必须产生 `RunPreempted` 和明确 interrupted outcome 或 recovery action。
- 外部控制必须携带当前 `AgentStepId`，过期 step id 必须被拒绝。

## RISC-like AgentAction 指令集

`AgentAction` 是底层精简指令集。每条指令只做一件事，且语义稳定。action 可以携带 typed operands；RISC-like 的重点是指令少、语义单一、操作数明确，而不是所有 action 都必须无参。

建议第一版：

```rust
pub enum AgentAction {
    LoadTools,
    BuildRequest {
        tools: Vec<ToolDefinition>,
    },
    OpenModelStream {
        request: GenerateRequest,
    },
    ReadModelStream,
    CommitAssistant {
        content: Vec<OutputContent>,
        reason: FinishReason,
        usage: Option<Usage>,
    },
    PlanTool {
        tool: ToolCall,
    },
    StartTool {
        tool: ToolCall,
        policy: ToolExecutionPolicy,
    },
    ReadTool {
        tool: ToolCall,
    },
    CommitToolResult {
        tool: ToolCall,
        result: ToolResult,
    },
    FinishTurn,
}
```

指令语义：

```text
LoadTools                    读取当前可用 tool definitions
BuildRequest { tools }       基于 context history/config 和 tools 构造 GenerateRequest
OpenModelStream { request }  用 request 打开 model stream
ReadModelStream              从 model stream 读取一个 OutputStreamEvent
CommitAssistant { .. }       将 assistant output 写入 context history
PlanTool { tool }            准备一个 tool call，并确定执行策略或拒绝结果
StartTool { tool, policy }   启动一个 tool call
ReadTool { tool }            读取一个 tool execution result
CommitToolResult { .. }      将 tool result 写入 context history
FinishTurn                   结束当前 turn
```

operand 规则：

- operand 是 action 的显式输入，不是外部控制命令。
- operand 必须是 immutable snapshot、业务值或稳定 id。
- operand 可以来自 `AgentTurn` 的 pending 数据或上一个 `AgentOutcome`。
- operand 不能包含 `&mut AgentContext`、`ModelStream`、task handle、cancellation token 等 runtime handle。
- operand 不能绕过 `AgentRun` 的 history ownership。
- 如果一个 operand 需要同时表达多种业务语义，应该拆 action，而不是扩大 operand。
- 允许 operands 的目的，是减少隐式 pending state 和 API 约定，而不是把执行层状态塞回 `AgentTurn`。

指令设计约束：

- `ReadModelStream` 每次最多处理一个 stream event。
- `CommitAssistant` 是唯一写入 assistant message 的指令。
- `CommitToolResult` 是唯一写入 tool message 的指令。
- `PlanTool` 只处理单个 tool call；多个 tool call 产生多个 step。
- 并行 tool execution 通过多次 `StartTool` 加多次 `ReadTool` 表达，而不是引入粗粒度 batch 指令。
- 新能力优先通过新增 action 或 action operand 表达，不把多种语义塞进一个 action。

tool identity 规则：

- `ToolCall.id` 使用 `ToolCallId` newtype，是 tool execution identity。
- `ToolResult.call_id` 同样使用 `ToolCallId`，必须等于对应的 `ToolCall.id`。
- 同一个 `AgentTurn` 内 `ToolCall.id` 必须唯一。
- 重复 `ToolCall.id` 视为 model/protocol error。
- `ReadTool` / `CommitToolResult` 直接使用 `ToolCall` 或 `ToolCall.id` 定位，不额外引入 `AgentToolRunId`。
- 只有支持 retry、sub-execution、跨 run 复用 tool call id 等场景时，才重新评估是否需要额外 execution id。

## AgentOutcome

`AgentOutcome` 是 `AgentAction` 执行后的结果。它是 `AgentTurn` 推进的输入，也是 `AgentEvent` 的来源。

建议结构：

```rust
pub enum AgentOutcome {
    ToolsLoaded(Vec<ToolDefinition>),
    RequestBuilt(GenerateRequest),
    ModelStreamOpened,
    ModelDelta {
        content_index: usize,
        delta: OutputContentDelta,
    },
    ModelOutputFinished {
        content: Vec<OutputContent>,
        reason: FinishReason,
        usage: Option<Usage>,
    },
    AssistantCommitted {
        message_index: usize,
        content: Vec<OutputContent>,
        reason: FinishReason,
        usage: Option<Usage>,
    },
    ToolPlanned {
        tool: ToolCall,
        policy: ToolExecutionPolicy,
    },
    ToolRejected {
        tool: ToolCall,
        result: ToolResult,
    },
    ToolStarted {
        tool: ToolCall,
    },
    ToolFinished {
        tool: ToolCall,
        result: ToolResult,
    },
    ToolResultCommitted {
        message_index: usize,
        tool: ToolCall,
        result: ToolResult,
    },
    TurnFinished,
    ActionInterrupted {
        reason: AgentInterruptReason,
    },
}
```

outcome 规则：

- outcome 是 action 的事实结果，不是外部控制命令。
- `AgentControl` 可以在 boundary 改写 pending outcome，再 commit 成最终 outcome。
- `AgentEvent` 只从最终 outcome 生成。
- committed outcome 必须不可变，用于 trace/replay。

## AgentControl

`AgentControl` 表示外部系统对当前 step boundary 的运行控制。它不是 agent 自己的业务判断。

使用场景：

- UI 让用户确认或改写某个 step。
- Orchestrator 暂停当前 agent，切换调度另一个 agent。
- Safety layer 丢弃 delta、拒绝工具、替换工具结果。
- Debugger 在每个 step 上单步推进。

建议结构：

```rust
pub enum AgentControl {
    Continue,
    Pause,
    AbortTurn,
    AbortRun,
    ReplaceRequest(GenerateRequest),
    ReplaceModelDelta(OutputContentDelta),
    DropModelDelta,
    ReplaceAssistantOutput(Vec<OutputContent>),
    ReplaceToolCall(ToolCall),
    RejectToolCall { reason: String },
    ReplaceToolResult(ToolResult),
}
```

control 规则：

- `AgentControl` 只作用于当前 `AgentStepId`。
- stale control 必须返回错误，不能修改 run state。
- `Continue` 使用当前 pending outcome。
- `Pause` 先完成当前 step，再让 run 停在下一步之前。
- `AbortTurn` 结束当前 turn，必要时进入 recovery。
- `AbortRun` 终止整个 run，必要时进入 recovery。
- `Preempt` 不是 boundary control，而是 run command，因为它可以中断 in-flight action。
- 不同 action/outcome 只接受对应 control，不匹配的 control 必须返回错误。

control 合法性建议：

```text
LoadTools/ToolsLoaded             -> Continue / Pause / AbortTurn / AbortRun
BuildRequest/RequestBuilt         -> Continue / Pause / AbortTurn / AbortRun / ReplaceRequest
ReadModelStream/ModelDelta        -> Continue / Pause / AbortTurn / AbortRun / ReplaceModelDelta / DropModelDelta
ReadModelStream/ModelOutputFinished -> Continue / Pause / AbortTurn / AbortRun / ReplaceAssistantOutput
PlanTool/ToolPlanned              -> Continue / Pause / AbortTurn / AbortRun / ReplaceToolCall / RejectToolCall
PlanTool/ToolRejected             -> Continue / Pause / AbortTurn / AbortRun / ReplaceToolCall / RejectToolCall
ReadTool/ToolFinished             -> Continue / Pause / AbortTurn / AbortRun
CommitAssistant/AssistantCommitted -> Continue / Pause / AbortRun
CommitToolResult/ToolResultCommitted -> Continue / Pause / AbortRun / ReplaceToolResult
FinishTurn/TurnFinished           -> Continue
```

## AgentEvent

`AgentEvent` 是观察协议，不是控制协议。

建议结构：

```rust
pub enum AgentEvent {
    RunStarted { run_id: AgentRunId },
    RunPaused { run_id: AgentRunId, at: AgentStepId },
    RunResumed { run_id: AgentRunId, at: AgentStepId },
    RunPreempted { run_id: AgentRunId, at: AgentStepId },
    RunRecovering { run_id: AgentRunId, after: AgentStepId },
    RunFinished { run_id: AgentRunId },
    RunAborted { run_id: AgentRunId },

    TurnStarted { run_id: AgentRunId, turn_id: AgentTurnId },
    TurnFinished { run_id: AgentRunId, turn_id: AgentTurnId },

    StepReady { step: AgentStep },
    StepStarted { step: AgentStep },
    ControlRequired { step: AgentStep, outcome: AgentOutcome },
    StepCompleted { step: AgentStep, outcome: AgentOutcome },

    ModelDelta {
        step_id: AgentStepId,
        content_index: usize,
        delta: OutputContentDelta,
    },
    AssistantCommitted {
        step_id: AgentStepId,
        message_index: usize,
        content: Vec<OutputContent>,
        reason: FinishReason,
        usage: Option<Usage>,
    },
    ToolStarted {
        step_id: AgentStepId,
        tool: ToolCall,
    },
    ToolResultCommitted {
        step_id: AgentStepId,
        message_index: usize,
        tool: ToolCall,
        result: ToolResult,
    },
}
```

event 规则：

- `AgentEvent` 只描述已经发生或正在等待外部控制的事实。
- `StepReady` 表示下一条 action 已分配 step id，但尚未进入 in-flight。
- `ControlRequired` 只表示 run 等待 `AgentControl`，不代表 control 本身。
- `StepCompleted` 只表示 control 已应用后的 final outcome，不承载 pending outcome。
- `RunRecovering` 表示 run 正在补齐必须提交的 recovery step，例如 aborted tool result。
- `ModelDelta` 必须是 `DropModelDelta` / `ReplaceModelDelta` 应用后的最终 delta。
- `AssistantCommitted` 必须在 assistant message 已写入 context history 后发出。
- `ToolResultCommitted` 必须在 tool result message 已写入 context history 后发出。
- 上层 UI 可以自行过滤 step-level event，但框架不再提供旧式 streaming event 兼容层。

## 驱动模式

底层需要同时支持自动执行和外部单步调度。

可替换调度器通过组合实现，不作为核心 trait 强制注册到 `copro-agent`。核心只提供可控 run 协议；任何 scheduler、orchestrator、UI 或 debugger 都是 `AgentRunHandle` 的使用方。

### Auto mode

`AgentRun` 内部自动对每个 boundary 使用 `AgentControl::Continue`，直到 turn/run 完成或遇到 configured control gate。

适合：

- 当前 `run_stream()` 自动执行 API。
- 普通 chat agent。
- 不需要外部逐步调度的场景。

### Controlled mode

`AgentRun` 每推进到 step boundary 都可以返回 `ControlRequired`，外部必须发送 `AgentControl` 才继续。

适合：

- 多 agent 调度器。
- 人工审批。
- debugger / tracer。
- safety layer。

调度器边界：

- 调度器可以决定何时调用 `step()`、`step_until_control()`、`control()`、`pause()`、`resume()`、`preempt()`。
- 调度器可以监听 `AgentEvent`，也可以在 `AgentCheckpoint` 上 match pending outcome 并提交 `AgentControl`。
- 调度器不能直接修改 `AgentTurn`、`AgentRunState`、in-flight handle 或 `AgentContext` history。
- 调度器不能提交 stale `AgentControl`；每次 control 必须携带当前 `AgentStepId`。
- `AgentCheckpoint` variant 保留细粒度切入点；`control()` 会立即拒绝非法 control kind 和非法 replacement invariant。
- 同一个 `AgentRunHandle` 同一时间只能有一个 active driver；`events()` 持有 stream lease，`step()` 持有单次调用 lease。
- 调度器 trait 如果需要，放在上层 crate，例如 `AgentRunDriver` / `AgentOrchestrator`，不进入底层核心。

示例 API：

```rust
pub trait AgentRunDriver: Send + Sync {
    async fn drive(&self, run: AgentRunHandle) -> Result<AgentRunSummary>;
}

impl AgentRunHandle {
    pub async fn step(&self) -> Result<AgentStepReport>; // auto-continue one step
    pub async fn step_until_control(&self) -> Result<AgentCheckpoint>;
    pub async fn control(&self, step_id: AgentStepId, control: AgentControl) -> Result<AgentStepReport>;
    pub async fn pause(&self) -> Result<()>;
    pub async fn resume(&self) -> Result<()>;
    pub async fn preempt(&self) -> Result<()>;
    pub async fn state(&self) -> Result<AgentRunState>;
}
```

内部实现可以选择 actor/task/channel，但这些是实现细节，不进入领域模型。

## Pause / Resume / Preempt / Recovery

### Pause

pause 是 boundary 行为。

规则：

- 如果 run 已在 boundary，立即进入 `Paused`。
- 如果 run 正在 in-flight，记录 pause request，当前 step 完成后进入 `Paused`。
- pause 不取消 in-flight action。
- pause 不回滚已经 committed 的 outcome。

### Resume

resume 从 `Paused { at }` 继续。

规则：

- 恢复时保持同一个 `AgentRunId`。
- 恢复时不重放已经 completed 的 step。
- 恢复从下一条 `AgentAction` 开始。

### Preempt

preempt 是 in-flight 中断。

规则：

- 可以在 `InFlight` 中接收。
- 取消当前 model stream 或 tool task。
- 当前 step 产生 `AgentOutcome::ActionInterrupted` 或进入 recovery。
- preempt 后必须停在明确边界：`Paused`、`Recovering`、`Finished` 或 `Aborted`。

### Recovery

recovery 由 `AgentRun` 负责，不由 `AgentTurn` 或外部 controller 临时拼接。

规则：

- model stream 中断且 assistant 未 commit：丢弃 partial output。
- assistant message 已 commit 且包含 tool call：必须补齐对应 tool result，保持 history 合法。
- tool execution 中断：如果 tool call 已经进入 history，生成 aborted tool result 并通过 `CommitToolResult` 写入。
- recovery action 也走普通 `AgentStep` clock，不能隐藏写 history。

## History Ownership

history 写入只能通过 `AgentRun` 对 `AgentContext` 的受控 commit 完成。

唯一合法写入点：

```text
AgentAction::CommitAssistant
AgentAction::CommitToolResult
explicit context mutation API when no active mutable run exists
```

禁止：

- `AgentTurn` 直接 push message。
- tool router 直接 push message。
- external controller 直接改 run 内部 pending history。
- active run 中并发执行 `replace_messages`。

第一版 mutation 规则：

- `push_message` / `replace_messages` / options/tool config mutation 只在没有 active mutable run 时允许。
- 如果 run `Paused` 且需要修改 context，必须通过 `AgentControl` 或显式 abort 当前 run 后再修改。
- 后续如需 paused edit，需要设计 snapshot/rebase，不在第一版隐式支持。

## 多 Agent 调度边界

底层框架只暴露可调度的 `AgentRun`，不内置多 agent orchestration 策略。

外部 orchestrator 可以做：

```text
agent_a_run.step_until_control()
agent_b_run.step_until_control()
agent_a_run.control(step_id, AgentControl::Pause)
agent_c_run.step()
```

框架保证：

- 每个 run 都有稳定 step clock。
- 每个 run 都能在 boundary 暂停。
- 每个 in-flight action 都能被 preempt 请求中断。
- 每个 committed outcome 都可以用于 trace。

框架不负责：

- agent 间任务分配。
- coding agent 的 planner/executor 分层。
- agent 角色、权限、团队拓扑。
- 分布式调度。

这些属于上层 `AgentPolicy` / `AgentPlanner` / `AgentOrchestrator`。

## 业务判断扩展点

`AgentControl` 不是 agent 业务判断。业务判断应该放在上层扩展点中。

候选概念：

```text
AgentPolicy     // 判断是否允许某个 action/outcome，可能产出 AgentControl
AgentPlanner    // 根据目标规划 agent 行为
AgentStrategy   // 对 model/tool/memory 使用策略进行配置
```

这些扩展点可以监听 `AgentEvent` 或接管 controlled mode，但不能绕过 `AgentRun` 的 step boundary 和 history ownership。

## run_stream 核心事件流

`run_stream()` 直接输出新的核心 `AgentEvent`，不再提供旧式 streaming 事件兼容映射。

规则：

- 内部创建 `AgentRun`。
- 使用 auto mode 自动推进。
- 输出 `StepReady` / `StepStarted` / `ControlRequired` / `StepCompleted` 等 step-level event 和 committed domain event。
- 不保留旧 `AgentEvent` / `LegacyAgentEvent` 兼容类型。

## 破坏性迁移计划

### Phase 1: 引入领域类型

- 新增 `AgentContext`、`AgentRun`、`AgentTurn`、`AgentStepId`、`AgentAction`、`AgentOutcome`、`AgentControl`。
- 使用新的核心 `AgentEvent` 替换旧式 streaming 事件。
- `run_stream()` 直接输出核心事件，作为破坏性迁移的一部分。

### Phase 2: 拆分长期状态和执行状态

- 把 `AgentState` 拆为 `AgentContext` 和 `AgentRun` 内部状态。
- 删除长期领域概念中的 `AgentActor` 命名。
- 同一个 context 限制一个 active mutable run。

### Phase 3: 重写 turn 状态机

- `TurnStepper` 改为 `AgentTurn`。
- 删除 `AgentTurn` 对 `messages` mutable reference 的依赖。
- 删除 `AgentTurn` 对 `ModelStream` 的持有。
- 删除 `AgentTurn` 对旧事件的直接产出。
- 改为 `next_action()` / `apply_outcome()`。

### Phase 4: 实现 AgentRun step clock

- `AgentRun` 执行 `AgentAction`。
- 所有 in-flight IO 移到 `AgentRun`。
- 每个 step 产出 `AgentOutcome`。
- 每个 committed outcome 产出新 `AgentEvent`。

### Phase 5: 暴露可调度 run API

- 新增 `AgentRunHandle`。
- 新增 `Agent::start_run()`。
- 实现 `step()`，一次推进一个 `AgentAction` 并返回 `AgentStepReport`。
- 实现 `step_until_control()`，在 configured control gate 处返回 `AgentCheckpoint`。
- `run_stream()` 改为基于 `AgentRunHandle` auto mode 的薄封装。
- 不引入核心 `AgentScheduler` trait；可替换调度器作为外部 `AgentRunHandle` driver 实现。

### Phase 6: 替换 hook/control 模型

- 从 `copro-agent` 核心移除传统 `AgentHook`。
- 在 step boundary 支持 `AgentControl`。
- request/output/tool/result 改写逻辑通过 `AgentRunHandle` control 实现。
- stale control 和非法 control 必须有测试。

### Phase 7: Pause / Resume / Preempt / Recovery

- 添加 pause/resume/preempt command。
- 实现 model stream cancellation。
- 实现 tool task cancellation。
- 实现 assistant tool call 已 commit 后的 recovery。
- 增加 history 合法性测试。

### Phase 8: 兼容层收敛

- `run_stream()` 直接输出核心 `AgentEvent`。
- 移除旧事件兼容映射。
- 旧 hook API 已移除。

## 当前非目标

- 不在第一版实现跨进程持久化恢复。
- 不在第一版允许同一 `AgentContext` 多个 active mutable run 并发写 history。
- 不在第一版设计 context fork/snapshot/rebase。
- 不在底层框架内置 coding agent planner。
- 不在底层框架内置多 agent orchestration 策略。
- 不把 scheduler/plugin trait 做成核心领域模型。
- 不让 `AgentTurn` 成为 async 类型。
- 不把传统 hook API 继续做成主要扩展点。
