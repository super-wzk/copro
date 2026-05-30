# copro-agent Migration Plan

## 目标

将 `copro-agent` 的执行模型收敛到 step-clock + `AgentTurnHandle` + `AgentControl`，彻底脱离旧 streaming/hook 扩展模型，并保证外部 scheduler/UI/debugger 可以可靠地在 step boundary 驱动 agent turn。

## 当前状态

已完成：

- `AgentTurnHandle` 已提供 `step_until_control()`、`events()`、`pause()`、`resume()`、`preempt()`；boundary control 通过一次性 `AgentControlPoint::control()` 提交。
- `AgentEvent` 已切换为核心 step-level 事件。
- `ControlRequired` / `StepCompleted` 已分离：前者表示 pending boundary，后者只表示 control 应用后的 final outcome。
- typed control point façade 已收敛为 `AgentControlPoint` + `AgentCheckpoint` enum，variant 保留细粒度切入点。
- `AgentControlPoint::control()` 已对非法 control kind 和 replacement invariant 做即时校验。
- 普通 `AgentEvent` 内部投递已移除 ack；只有 `ControlRequired` 携带 reply 并阻塞 turn 等待 `AgentControl`。
- `ToolResultReplacement` 已用于 typed tool result replacement，由运行层自动填充 `call_id` / `name`。
- `Pause` / `Resume` 已形成 boundary 和 in-flight pause request 的 `TurnPaused` -> `TurnResumed` 事件链。
- `FinishTurn` / `AbortTurn` 已在事件层区分，in-flight `preempt()` 已产生 `TurnPreempted`。
- `Ready` 已通过 `StepReady` 成为真实可观察状态，`Recovering` 已通过 `TurnRecovering` 成为真实 recovery 状态。
- abort/preempt/stop 遇到已提交 tool call 或 tool execution boundary 时，会通过 recovery step 补齐 tool result。
- `AgentTurnHandle` 已加入单 driver lease，避免 `events()` / `step_until_control()` 并发抢同一 receiver。
- `AgentTurnMachine` 已收敛为 `next_action()` / `apply_outcome()` 纯同步状态机；`AgentTurn` 统一按 action step loop 执行 async IO、control boundary、commit 和 event emission。
- turn 执行模块已拆为 `turn/{types,control,checkpoint,handle,execution}.rs`，`turn/mod.rs` 只保留声明和 re-export。
- `runtime` public namespace 已移除；`StopSignal` public API 已移除，取消能力收拢为每个 turn 内部的 cancellation source，由 `AgentTurnHandle` 的 `AbortTurn` / `preempt()` 驱动。
- `AgentHistory` 已作为 public 可序列化 history 值对象加入 API，用于保存/恢复长期对话历史；字段私有，常规应用层只通过 `push_input(InputMessage)` 追加输入，不包含 model/tools 或 in-flight turn/runtime 对象。
- `AgentTurnConfig` 已作为 public 可序列化 turn 配置值对象加入 API，用于保存/复用 options/tool_choice/hosted_tools。
- `AgentTurnHandle::events()` 已作为 core event stream，`run_stream()` convenience API 已移除，应用层通过 `start_turn(history, config, model, tools)` 获取 handle。
- public `Agent` facade 已移除；应用层拥有 `AgentHistory`，每个 turn 消费 history，并在完成后通过 `AgentTurnHandle::into_history()` 取回更新后的 history。
- `AgentControl` 已支持 request、model delta、assistant output、tool call、tool result 的改写/拒绝。
- `AgentHook` / `AgentHooks` / `ToolCallDecision` 已从当前工作区代码中移除。
- `copro-harness` 的 skills 注入已迁移为显式 `SkillRequestInjector`。
- `simple-cli` 已通过 `RequestBuilt` boundary 使用 `ReplaceRequest` 注入 skills request。
- `cargo clippy`、`cargo test -p copro-agent`、`cargo test`、RustRover build 已在当前迁移过程中通过。

## 剩余风险

### 1. ControlRequired / StepCompleted 语义未分离（已完成）

原问题：

- 当前 `StepCompleted` 既表示 pending control boundary，又被外部当作 step 完成事件。
- `Replace*` / `Drop*` control 后，已发出的 `StepCompleted` outcome 可能不是最终 outcome。
- `ControlRequired` 已定义但尚未真正发出。

目标：

- `ControlRequired { step, outcome }` 表示 pending outcome，等待外部 control。
- `StepCompleted { step, outcome }` 只表示 control 已应用后的 final outcome。
- history commit 和 domain event 必须基于 final outcome。

验收：

- 替换 request/delta/output/tool call/tool result 后，`StepCompleted` 中只出现最终 outcome。
- `events()` auto mode 仍能自动 `Continue`。
- `step_until_control()` 返回 control point 时不再把 pending outcome 伪装成 completed；默认继续由 `AgentControlPoint` drop 兜底或显式 `continue_turn()` 完成。
- 新增测试覆盖 `ControlRequired -> control -> StepCompleted` 顺序。

### 2. control() 合法性校验滞后（已完成）

原问题：

- stale `AgentStepId` 不再暴露给应用层作为 control 参数。
- 旧 API 下非法 `AgentControl` 仍可能在 `control()` 返回 `Ok` 后，由下一次驱动暴露错误。
- 当前 API 允许外部组合“任意 `AgentControl` + 任意 step”，control kind 和 step outcome 的非法组合只能靠运行时发现。

目标：

- 优先通过 Rust 类型系统防止 control kind 和 step outcome 的非法组合。
- `AgentControlPoint::control()` 基于当前 pending outcome 的 allowed controls 立即拒绝非法 control。
- 运行层仍保留防御性校验，避免绕过 handle 时进入非法状态。

方向：

- 使用单个 `AgentCheckpoint` enum 表达 typed checkpoint，例如 `RequestBuilt(report)`、`ModelDelta(report)`、`ToolResult(report)`。
- 外部通过 `point.checkpoint()` match variant 获取对应 pending 数据，再用同一个 `AgentControlPoint` 提交 `AgentControl`。
- 不再为每个 checkpoint 维护单独 wrapper struct，避免 API 和 runner 代码膨胀。
- 保留低层 escape hatch 时必须走 immediate runtime validation。

验收：

- 非法 control 在 `AgentControlPoint::control()` 返回 `Err`。
- 旧的“下一次 step 才报错”测试改为 immediate error。
- stale control token 不暴露给应用层；单个 `AgentControlPoint` 不可 clone 且持有 driver lease。
- 新增 `AgentCheckpoint` enum 路径，常规调用围绕 checkpoint variant match 写控制逻辑。
- 测试覆盖 checkpoint 合法路径，以及低层 API 的非法 control immediate error。

### 3. Pause / Resume 语义不完整（已完成）

原问题：

- `TurnPaused` / `TurnResumed` 事件没有完整事件链。
- `pause()` 当前只修改 handle 本地状态，不一定驱动 turn 发出 pause event。
- `resume()` 只是发送 `Continue`，没有 `TurnResumed`。
- in-flight pause request 还没有实现。

目标：

- boundary pause 由 turn 发出 `TurnPaused`。
- resume 发出 `TurnResumed` 后继续运行。
- 如果 turn 正在 in-flight，pause request 在当前 step 完成后生效。

验收：

- `AgentControlPoint::control(Pause)` 产生 `TurnPaused`，turn 停在当前 boundary 后。
- `resume()` 产生 `TurnResumed`，随后继续下一个 step。
- in-flight model stream/tool execution 场景下 pause 不取消当前 action，只在 boundary 停住。

### 4. Preempt / Abort / Recovery 语义较弱（已完成）

原问题：

- `TurnPreempted` / `Recovering` 目前偏预留。
- `FinishTurn` 和 `AbortTurn` 行为仍接近。
- assistant 已 commit tool call 后，如果 abort/preempt 发生在某些 boundary，可能留下无 tool result 的 history。

目标：

- `FinishTurn` 只结束当前 turn，并按需补齐 tool result recovery。
- `AbortTurn` 终止 turn，并按需补齐 tool result recovery。
- `preempt()` 可中断 in-flight action，并发出明确 interrupted/recovery event。

验收：

- assistant tool call 已 commit 后，无论 abort/preempt 发生在哪里，history 中每个 tool call 都有对应 tool result。
- `FinishTurn` / `AbortTurn` 事件顺序有测试覆盖。
- in-flight model stream 和 tool execution 的 preempt 行为有测试覆盖。

### 5. AgentTurnHandle 缺少单 driver 保护（已完成）

原问题：

- `AgentTurnHandle` 可 clone。
- `events()` / `step_until_control()` 共享同一个 receiver，多个消费者可能抢事件。
- mutex 避免数据竞争，但不保证执行语义正确。

目标：

- 一个 turn 同时只能有一个 active driver。
- `events()` 和 `step_until_control()` 不能并发消费同一个 turn。

验收：

- 第二个 active consumer 会立即返回清晰错误。
- dropped consumer 能释放 driver lease。
- 测试覆盖 `events()` 与 `step_until_control()` 并发/交替误用。

### 6. 替换 control 缺少一致性校验（已完成）

原问题：

- `ReplaceToolResult` 不验证 `call_id` / `name` 是否匹配原 tool。
- `ReplaceToolCall` 不检查同 turn 内 `ToolCall.id` 唯一。
- `ReplaceAssistantOutput` 可以替换出和 `FinishReason` 不一致的内容。

目标：

- 替换值必须保持协议合法。
- 明确哪些字段允许改写，哪些字段必须保持一致。
- 尽量用 API 类型避免外部传入本不该修改的字段。

方向：

- 对 `ReplaceToolResult` 优先提供 `ToolResultReplacement`，只让外部提供 `status` / `content`，由运行层从当前 tool 自动填充 `call_id` / `name`。
- 对 `ReplaceToolCall` 提供专用 replacement builder 或 validator，运行时检查同 turn 内 `ToolCall.id` 唯一。
- 对 `ReplaceAssistantOutput` 基于当前 `FinishReason` 做运行时校验，拒绝 `FinishReason::Stop` 搭配 tool call output 等不一致组合。
- 类型系统负责“不能调用不适用的 replacement 方法”，运行时负责“replacement value 与当前 turn state 一致”。

验收：

- `ReplaceToolResult` 的 `call_id` 和 `name` 不匹配时立即拒绝。
- `ReplaceToolCall` 的 id 与同 turn 其他 tool call 重复时立即拒绝。
- `FinishReason::Stop` + tool call output 等不一致组合会被拒绝。
- 测试覆盖合法和非法替换。
- 常规 typed API 下无法构造 `call_id` / `name` 不匹配的 `ToolResult` replacement。

### 7. AgentTurnMachine 仍未完全纯状态机化（已完成）

原问题：

- 旧实现中 `AgentTurn` 直接按 phase 驱动请求、模型流、工具规划、工具执行和结果提交。
- 文档中的 `next_action()` / `apply_outcome()` 尚未成为实际实现。

目标：

- `AgentTurnMachine` 只负责纯状态推进。
- `AgentTurn` 负责 async IO、control、commit、event。
- action 选择和 outcome 应用尽量收敛到 `AgentTurnMachine`。

验收：

- `AgentTurnMachine` 无 async、无 IO handle、无 cancellation token。
- `AgentTurn` 不再按 `AgentTurnMachinePhase` dispatch；统一循环为 `next_action()` -> execute -> control -> commit/apply outcome。
- 现有行为测试继续通过。

### 8. core event stream 的产品语义（已收敛）

原问题：

- `run_stream()` 直接输出核心 `AgentEvent`，包含 `StepReady` / `StepStarted` / `ControlRequired` / `StepCompleted` 等 step-level 事件。
- 该 convenience API 隐藏了 `AgentTurnHandle`，和应用层显式控制 turn 的模型冲突。

目标：

- 移除 `run_stream()`，统一通过 `start_turn(history, config, model, tools)` 获取 `AgentTurnHandle`。
- `AgentTurnHandle::events()` 作为 core stream 的自动驱动入口。
- 如需 high-level chat stream，新增 adapter，而不是恢复旧兼容层。

验收：

- 文档明确 `start_turn()` / `events()` 语义。
- CLI 和示例使用正确 stream 层级。
- 如果新增 high-level adapter，确保不影响 core `AgentEvent`。

## 实施顺序

### Phase A: 提交当前迁移基线

内容：

- 提交 hook 移除、skills injector 迁移、clippy 修复和本文档。

验收：

- `git status --short` 干净。
- `cargo clippy` 通过。
- `cargo test` 通过。
- RustRover build 通过。

### Phase B: 分离 ControlRequired / StepCompleted

内容：

- 修改事件发送 helper。
- `emit_step_completed` 改为先发 `ControlRequired`，应用 control 后再发 `StepCompleted(final)`。
- 调整 `AgentTurnHandle::step_until_control()` 以 `ControlRequired` 作为 control point；移除 `step()`，避免和 control point drop 默认继续语义重复。
- `events()` auto mode 对 `ControlRequired` 自动 `Continue`。

验收：

- 所有改写 control 的 final `StepCompleted` outcome 正确。
- 相关测试覆盖事件顺序。

### Phase C: 立即 control 校验和替换一致性校验

内容：

- 引入 `AgentCheckpoint` enum，让常规控制逻辑围绕 typed checkpoint variant 编写。
- `AgentControlPoint::control()` 直接基于 allowed controls 校验。
- 增加 request/output/tool replacement validator 和专用 replacement 类型。
- 保留运行层防御性校验。

验收：

- 非法 control 和非法 replacement 都在 `AgentControlPoint::control()` 返回 `Err`。
- 常规 checkpoint API 通过 variant match 降低非法 control 组合风险，control point 继续即时拒绝非法组合。
- `ToolResultReplacement` 等专用类型避免外部传入不一致 identity 字段。
- 测试覆盖每种非法替换。

### Phase D: Pause / Resume 完整化

内容：

- 增加 pause request 状态。
- turn 在 boundary 发 `TurnPaused`。
- resume 发 `TurnResumed`。

验收：

- boundary pause/resume 和 in-flight pause 均有测试。

### Phase E: Abort / Preempt / Recovery

内容：

- 区分 `FinishTurn` 和 `AbortTurn`。
- 对 pending tool calls 生成 recovery tool results。
- preempt 支持 in-flight cancellation 和明确 event。

验收：

- history 合法性测试覆盖 tool-call commit 后的 abort/preempt。

### Phase F: 单 driver 保护

内容：

- 在 `AgentTurnHandle` 中加入 driver lease。
- `step_until_control()` / `events()` 进入消费前获取 lease。
- lease drop 后释放。

验收：

- 并发消费会立即报错。
- 正常单 driver 行为不变。

### Phase G: AgentTurnMachine 纯状态机收敛

内容：

- 引入 `next_action()` / `apply_outcome()`。
- 移除 `AgentTurn` 中的 phase dispatch，改为统一 action step loop。

验收：

- `AgentTurnMachine` 保持纯同步状态机。
- 行为测试不回退。

## 每阶段固定验证

每个 phase 完成后执行：

```sh
cargo fmt
cargo clippy
cargo test -p copro-agent
cargo test
```

同时执行 RustRover build，确保 IDE 侧也无编译问题。

## 提交策略

- 每个 phase 独立提交。
- 行为语义变更和纯文档/格式修复分开提交。
- 不把未验证的中间状态提交到主分支。
