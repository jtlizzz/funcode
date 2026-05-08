# 模型-工具循环当前状态与下一步计划

日期：2026-04-28

## 目标边界

当前阶段目标不是完整的终端编程助手 MVP，而是先把最小的模型-工具循环跑通，并保持抽象足够简洁、严谨。

最小闭环定义为：

```text
用户输入
  -> Agent 追加 User item
  -> Session 构建 ModelRequest
  -> Model 真实流式调用 OpenAI-compatible provider
  -> 模型产生文本或 tool call
  -> Agent 执行 ToolRegistry 中的工具
  -> Agent 追加 ToolResult
  -> Session 再次构建请求
  -> 模型基于工具结果输出最终回答
  -> Bus 向 TUI/CLI 发送观察事件
```

当前阶段优先验证这个闭环，不急于实现 shell、写文件、审批、Git、持久化和复杂上下文管理。

## 当前项目状态

### 已完成的核心内核

- `src/model.rs` 已定义 canonical `Item` / `Message` / `ToolCall` / `ToolResult`，并实现 OpenAI-compatible provider。
- `src/model.rs` 中 `ModelProvider::stream()` 已支持 `CancellationToken`，provider 负责把 SSE 增量组装为权威完成态事件：`TextDone`、`ToolCallReady`、`Completed`。
- `src/session.rs` 已负责 system prompt、history、tools 的请求拼装，以及基础 token 预算截断。
- `src/tools.rs` 已提供 `Tool` trait 与 `ToolRegistry`，可以注册工具、导出 tool specs、按名称分发执行并封装 `ToolResult`。
- `src/agent.rs` 已实现核心循环：流式消费模型事件、转发观察事件、写入完成态 item、执行工具、把工具结果写回 session、继续下一轮。
- `src/bus.rs` 已提供基于 `tokio::sync::broadcast` 的事件总线，支持 TUI/CLI 订阅 `TextDelta`、`ToolCallBegin`、`ToolCallEnd`、`TurnComplete` 等事件。
- 单元测试已覆盖文本回复、工具调用、工具结果入 session、max turns、取消、协议错误等内核行为。

### 已有但仍偏原型的能力

- `Op::UserTurn` / `Op::Interrupt` 已存在，但 `submit(&mut self, Op::UserTurn)` 会 await 完整 turn，真实 UI 场景下还不能并发提交 interrupt。
- `CancellationToken` 已能取消模型流，但还不能取消已经开始的工具执行。
- `max_turns` 已能防止无限工具循环，但当前以 `Event::Error("max turns reached")` 表达，缺少结构化 turn outcome。
- `fs.rs` 已有底层 `FileSystem` trait 和 `LocalFs`，但还没有包装成模型可调用的 `Tool`。
- `Event::ApprovalRequired` 已定义，但审批流程尚未接入 Agent 或工具执行。

### 仍是骨架的模块

- `src/main.rs`：真实程序入口尚未接入应用启动流程。
- `src/app.rs`：尚未装配 `Config`、`Model`、`Session`、`ToolRegistry`、`Bus`、`Agent`。
- `src/config.rs`：尚未从环境变量或配置文件读取 API key、model、base URL、max turns、context tokens。
- `src/tui.rs`：当前只是命名后的交互层入口，还没有读输入、渲染 bus 事件、处理 Ctrl-C。
- `src/shell.rs`、`src/approval.rs`、`src/context.rs`、`src/git.rs`：当前阶段可以暂缓。

## 当前主要设计判断

### 1. 暂不把完整 MVP 范围全部纳入当前迭代

完整编码助手需要 shell、写文件、apply patch、审批、Git、上下文压缩、持久化等能力。但这些会显著扩大安全模型和交互复杂度。

当前迭代只做只读工具和真实模型联调，更利于验证核心抽象是否正确。

### 2. 优先使用 Tokio 原生消息原语，不引入 actor 框架

后续若要让 TUI 和 Agent 解耦，应优先采用：

- `tokio::sync::mpsc`：TUI -> Agent 的 `Op` 队列。
- `tokio::sync::oneshot`：单次 user turn 的 `TurnOutcome` 返回。
- `tokio::sync::broadcast`：Agent -> TUI 的观察事件；当前 `Bus` 已采用。
- `tokio_util::sync::CancellationToken`：当前 turn 的取消信号。

不建议当前引入 `actix`、`ractor`、`xtra` 等 actor 框架。项目只需要单个 Agent 后台任务，Tokio primitives 已足够。

### 3. 当前可先不 actor 化，但要明确它的触发条件

如果只是跑通真实模型-工具循环，可以继续保留 `Agent::submit(&mut self, Op::UserTurn)`。

当出现以下需求时，再改成 `AgentHandle + mpsc<Op> + actor loop`：

- TUI 需要在模型 streaming 时响应 Ctrl-C。
- 工具执行期间需要中断。
- 审批流程需要暂停当前 turn 并等待用户 `Approve` / `Reject`。
- 多个 UI/control source 需要同时向 Agent 提交操作。

## 下一步计划

### Step 1：实现最小配置加载

目标：程序能从环境变量启动真实模型 provider。

建议实现：

- `Config::from_env()`
- `OPENAI_API_KEY`：必填。
- `OPENAI_BASE_URL`：可选。
- `FUNCODE_MODEL`：可选，提供默认模型名。
- `FUNCODE_MAX_TURNS`：可选，默认 `10`。
- `FUNCODE_CONTEXT_TOKENS`：可选，默认一个保守值。

验收标准：

- 缺少 API key 时返回明确错误。
- model 为空时返回明确错误。
- `app.rs` 可以直接消费 `Config` 完成装配。

### Step 2：实现只读工具 `read_file`

目标：让模型至少能真实调用一个工具，验证 tool call -> tool result -> follow-up model call。

建议实现位置：仍放在 `src/tools.rs` 或 `src/fs.rs` 中，不新建工具子目录，符合当前单文件模块约束。

建议行为：

- 参数 schema：`{ "path": "string" }`。
- 路径限制在当前 workspace/cwd 内。
- 拒绝绝对路径逃逸和 `..` 逃逸。
- 读取 UTF-8 文本文件。
- 设置最大输出长度，超出时截断并在结果中标明。
- 只读工具不需要审批。

验收标准：

- `ToolRegistry::specs()` 能导出 `read_file`。
- 模型请求中能看到 `read_file` tool spec。
- 工具调用成功后，`Session` 中出现对应 `ToolResult`。
- 路径非法、文件不存在、非 UTF-8 或过大时返回可读错误，而不是 panic。

### Step 3：实现最小应用装配层

目标：把内核对象连起来，不做复杂生命周期管理。

建议 `app.rs` 暴露：

```rust
pub async fn run(config: Config) -> Result<(), AppError>
```

装配内容：

- `OpenAIProvider`
- `Model`
- `Session`
- `ToolRegistry`，先注册 `read_file`
- `Bus`
- `Agent`
- TUI/CLI loop

验收标准：

- `main.rs` 只负责加载配置、调用 `app::run()`、打印顶层错误。
- app 层不直接承担模型协议细节和工具执行细节。

### Step 4：实现最小 TUI/CLI loop

目标：先用 REPL 跑通交互，不追求完整 TUI。

建议行为：

- 从 stdin 读一行用户输入。
- 输入 `/exit` 或 EOF 退出。
- 调用 `agent.submit(Op::UserTurn(text)).await`。
- 订阅 `Bus` 并渲染：
  - `TextDelta`：直接流式打印。
  - `ToolCallBegin`：显示工具名。
  - `ToolCallEnd`：显示成功/失败和简短输出。
  - `TurnComplete`：换行并显示结束。
  - `Error`：显示错误。

验收标准：

- 能在终端输入一句话并看到真实模型流式回答。
- 能让模型调用 `read_file` 并基于工具结果继续回答。
- 不要求 Ctrl-C 中断；这留到 actor/op queue 改造时处理。

### Step 5：补结构化 turn outcome

目标：让上层不要只依赖 bus 事件判断控制流结果。

建议新增：

```rust
pub enum TurnOutcome {
    Completed { usage: Option<TokenUsage> },
    Interrupted,
    Failed(String),
    MaxTurnsReached { max_turns: usize },
}
```

短期可以让 `submit(Op::UserTurn)` 返回 `TurnOutcome`。后续 actor 化时，`Op::UserTurn` 可以携带 `oneshot::Sender<TurnOutcome>`。

验收标准：

- CLI/app 可以区分正常完成、中断、错误、达到 max turns。
- `max_turns` 不再只通过普通 error string 表达。
- bus 仍保留观察事件，不承担唯一控制流语义。

### Step 6：真实端到端验收

建议创建一个人工验收场景：

```text
用户：请读取 Cargo.toml，告诉我这个项目用了哪些核心依赖。
模型：调用 read_file({ "path": "Cargo.toml" })
Agent：执行 read_file，写入 ToolResult
模型：基于 ToolResult 回答依赖列表
```

验收标准：

- 至少出现一次真实 provider 的 tool call。
- 工具参数能从 provider streaming delta 正确组装。
- 工具结果能正确进入下一轮模型请求。
- 最终回答来自工具结果，而不是模型凭空猜测。

## 暂缓项

以下能力对完整 MVP 重要，但不是当前阶段跑通模型-工具循环的必要条件：

- `shell` 工具。
- `write_file` / `apply_patch`。
- 审批流和权限策略。
- Ctrl-C 真实并发中断。
- Agent actor/op queue 改造。
- Git status/diff 集成。
- 持久化会话。
- 复杂上下文压缩和项目文件选择。
- 完整 TUI 视觉与交互体验。

## 风险与注意事项

- 如果先实现 `shell` 或写文件，会被迫提前处理审批、超时、取消和输出截断，容易偏离当前目标。
- 如果长期保留 `submit(&mut self)`，`Op::Interrupt` 会继续存在真实并发不可用的问题；这需要在文档和代码注释中保持诚实。
- 如果 `read_file` 不做路径限制和输出截断，即便是只读工具也可能造成隐私或上下文爆炸问题。
- 如果 bus 事件继续承担控制流语义，上层会难以可靠判断 turn 的最终状态；应尽早引入 `TurnOutcome`。

## 推荐执行顺序

1. `config.rs`：最小 `Config::from_env()`。
2. `tools.rs` / `fs.rs`：只读 `read_file` tool。
3. `app.rs`：装配真实对象。
4. `tui.rs`：最小 REPL + bus 渲染。
5. `agent.rs`：`TurnOutcome`。
6. 真实模型端到端验收。
7. 再评估是否进入 actor/op queue、审批、shell、写文件阶段。
