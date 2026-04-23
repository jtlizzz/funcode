# funcode

一个 Rust 编写的终端 AI 编程助手。

## 核心目标

funcode 的核心是 **Agent Loop**——一个"模型调用 → 工具执行 → 模型继续推理"的循环：

1. 用户提出编程任务
2. 模型分析任务，决定是直接回答还是调用工具（读文件、编辑代码、执行命令等）
3. 如果调用了工具，将结果回传给模型，回到第 2 步
4. 模型认为任务完成时停止

整个系统围绕这个循环构建，所有模块都为它服务。

## 架构

参考 Codex 的 SQ/EQ（Submission Queue / Event Queue）模式。Agent 是一个独立的运行体，通过双向 channel 与 UI 通信。

```
┌──────────────┐    submit(Op)     ┌──────────────────────────────────┐
│              │ ─────────────────► │                                  │
│   CLI / UI   │                   │   Agent                          │
│              │ ◄───────────────── │                                  │
└──────────────┘    recv(Event)     │   ┌─ Session（消息历史 + 状态）  │
                                    │   ├─ Model   （模型调用）        │
                                    │   ├─ Tools   （工具执行）        │
                                    │   └─ Loop    （循环驱动）        │
                                    └──────────────────────────────────┘
```

- UI 只做一件事：提交 `Op`，接收 `Event`
- Agent 内部持有所有状态，驱动模型调用和工具执行的循环
- 双方通过 channel 解耦，同一个 Agent 可以对接不同的 UI（终端、Web、IDE 插件）

## 目录结构

```
src/
├── main.rs       程序入口
├── app.rs        应用启动、装配各模块
├── cli.rs        终端 UI，提交 Op、展示 Event
├── config.rs     配置加载（API key、模型、权限）
│
├── agent.rs      Agent 核心（spawn、submission_loop、run_turn）
├── model.rs      模型接入（统一消息类型、OpenAI 兼容 API、流式响应）
├── tools.rs      工具系统（Tool trait、注册中心、5 个内置工具）
│
├── session.rs    会话状态（消息历史、上下文裁剪）
├── context.rs    system prompt 组装
├── approval.rs   用户审批（高风险操作确认）
├── fs.rs         文件系统抽象
├── shell.rs      命令执行
├── git.rs        Git 集成
```

## 当前进度

- [x] 模型接入（`model.rs`）：统一消息类型、OpenAI 兼容 API、流式响应、流式累加器
- [x] 工具系统（`tools.rs`）：Tool trait、ToolRegistry、JSON Schema 自动生成、5 个具体工具
- [x] 文件系统（`fs.rs`）：异步文件读写、目录遍历
- [ ] Agent 核心（`agent.rs`）
- [ ] 会话状态（`session.rs`）
- [ ] 终端 UI（`cli.rs`）
- [ ] 配置加载（`config.rs`）
- [ ] 其他模块

---

# Agent 模块设计

## Op（用户指令）

```rust
pub enum Op {
    /// 用户发送一条消息，开始一轮对话
    UserTurn { content: String },

    /// 中断当前正在执行的 turn
    Interrupt,

    /// 回复审批请求（允许/拒绝）
    ApprovalResponse { id: String, approved: bool },

    /// 关闭 Agent
    Shutdown,
}
```

## Event（Agent 事件）

```rust
pub enum Event {
    // Turn 生命周期
    TurnStarted,
    TurnComplete { usage: Option<TokenUsage> },

    // 模型输出（流式）
    TextDelta(String),
    TextDone(String),

    // 工具调用
    ToolCallBegin { id: String, name: String },
    ToolCallEnd { id: String, name: String, output: String, is_error: bool },

    // 审批请求
    ApprovalRequired { id: String, tool_name: String, description: String },

    // 状态
    Error(String),
}
```

## Agent 公共接口

```rust
pub struct Agent {
    tx_sub: Sender<Submission>,
    rx_event: Receiver<Event>,
}

impl Agent {
    /// 创建并启动 Agent，返回 Agent 句柄
    pub fn spawn(config: AgentConfig) -> Self

    /// 提交一条指令
    pub fn submit(&self, op: Op)

    /// 接收下一个事件（异步）
    pub async fn next_event(&mut self) -> Option<Event>
}
```

UI 拿到 `Agent` 后只做 `submit` 和 `next_event`，不访问内部状态。

## 内部结构

`spawn` 内部创建 `AgentInner` 并启动一个 tokio task 运行 `submission_loop`：

```rust
struct AgentInner {
    session: Session,            // 消息历史 + 上下文
    model: Model,                // 模型客户端
    tool_registry: ToolRegistry, // 工具注册表
    tx_event: Sender<Event>,     // 事件输出
    max_turns: u32,              // 单轮对话最大循环次数
}
```

## submission_loop

```rust
async fn submission_loop(mut inner: AgentInner, mut rx_sub: Receiver<Submission>) {
    while let Some(sub) = rx_sub.recv().await {
        match sub.op {
            Op::UserTurn { content } => {
                inner.run_turn(content).await;
            }
            Op::Interrupt => {
                // 取消当前 turn（通过 CancellationToken）
            }
            Op::ApprovalResponse { id, approved } => {
                // 回复审批
            }
            Op::Shutdown => break,
        }
    }
}
```

## run_turn（核心循环）

一次 `UserTurn` 触发一次 `run_turn`，内部是模型调用 + 工具执行的循环：

```rust
async fn run_turn(&mut self, user_input: String) {
    // 1. 追加用户消息到 session
    self.session.push_message(ChatMessage::user(user_input));
    self.emit(Event::TurnStarted);

    for _ in 0..self.max_turns {
        // 2. 从 session 构建模型请求（含 system prompt、历史、工具定义）
        let request = self.session.build_request(&self.tool_registry);

        // 3. 流式调用模型
        let stream = match self.model.stream(request).await {
            Ok(s) => s,
            Err(e) => {
                self.emit(Event::Error(e.to_string()));
                break;
            }
        };

        // 4. 消费流：文本 delta 实时 emit，累积 tool calls
        let response = self.consume_stream(stream).await;

        // 5. 追加 assistant 消息到 session
        self.session.push_message(response.message);

        // 6. 提取工具调用
        let tool_calls = response.extract_tool_calls();
        if tool_calls.is_empty() {
            self.emit(Event::TurnComplete { usage: response.usage });
            break;
        }

        // 7. 执行工具调用
        for call in tool_calls {
            self.emit(Event::ToolCallBegin { id: call.id.clone(), name: call.name.clone() });

            // 审批检查（可写操作需要用户确认）
            if self.needs_approval(&call) {
                self.emit(Event::ApprovalRequired { ... });
                // 等待审批结果（从 rx_sub 读取）
                // 如果被拒绝，生成 error result
            }

            let result = self.tool_registry.execute(&call.id, &call.name, &call.arguments).await;
            self.emit(Event::ToolCallEnd { ... });

            // 8. 追加工具结果到 session
            self.session.push_message(ChatMessage::from(result));
        }

        // 9. 继续循环，让模型看到工具结果后决定下一步
    }

    self.emit(Event::TurnComplete { usage: None });
}
```

## consume_stream

消费模型流式响应，同时做两件事：

1. 文本 delta 通过 `Event::TextDelta` 实时 emit 给 UI
2. 累积完整的 tool calls，最终返回 `ModelResponse`

```rust
async fn consume_stream(&self, mut stream: ResponseStream) -> ModelResponse {
    let mut final_response = None;

    while let Some(result) = stream.next().await {
        match result {
            Ok(ResponseEvent::TextDelta(text)) => {
                self.emit(Event::TextDelta(text));
            }
            Ok(ResponseEvent::ToolCallStart { id, name }) => {
                self.emit(Event::ToolCallBegin { id, name });
            }
            Ok(ResponseEvent::ToolCallDone { .. }) => {}
            Ok(ResponseEvent::MessageDone(response)) => {
                final_response = Some(response);
            }
            Ok(_) => {}
            Err(e) => {
                self.emit(Event::Error(e.to_string()));
            }
        }
    }

    final_response.expect("stream should produce MessageDone")
}
```

## Session 的职责

Session 被 Agent 内部持有，不暴露给 UI。它做三件事：

1. **消息历史**：`push_message` / `messages`
2. **上下文裁剪**：`build_request` 从历史中构建 `ModelRequest`，含 system prompt + token 预算管理
3. **工具注册**：持有 `ToolRegistry` 的引用

```rust
struct Session {
    messages: Vec<ChatMessage>,
    system_prompt: String,
    max_tokens: usize,  // token 预算
}

impl Session {
    fn push_message(&mut self, msg: ChatMessage) { ... }
    fn build_request(&self, registry: &ToolRegistry) -> ModelRequest { ... }
}
```

`build_request` 的职责：
- 拼装 system prompt
- 从 `messages` 中按 token 预算裁剪历史（保留 system prompt + 最近 N 条）
- 附加 `registry.specs()` 作为工具定义

## 数据流示例

```
用户输入 "修复这个 bug"
  │
  ▼  submit(Op::UserTurn)
submission_loop 收到
  │
  ▼  run_turn()
  ├── session.push_message(user)
  ├── session.build_request() → ModelRequest
  ├── model.stream(request)
  │     │
  │     ├── TextDelta("让我看看代码...") → emit → UI 展示
  │     ├── ToolCallStart(Read, src/main.rs)
  │     └── MessageDone
  ├── session.push_message(assistant + tool_call)
  ├── tool_registry.execute(Read, ...) → "fn main() {}"
  │     └── emit ToolCallEnd → UI 展示结果
  ├── session.push_message(tool_result)
  │
  │  ── 继续循环 ──
  │
  ├── session.build_request()  // 这次历史里多了工具结果
  ├── model.stream(request)
  │     ├── TextDelta("找到了，修复如下...")
  │     ├── ToolCallStart(Edit, ...)
  │     └── MessageDone
  ├── 审批检查 → Edit 需要用户确认
  │     └── emit ApprovalRequired → UI 展示确认提示
  │     └── 用户确认 → submit(ApprovalResponse)
  │     └── 继续执行 Edit
  ├── session.push_message(tool_result)
  │
  │  ── 继续循环 ──
  │
  ├── model.stream(request)
  │     ├── TextDelta("已修复。")
  │     └── MessageDone (finish_reason: stop)
  ├── 无工具调用 → break
  └── emit TurnComplete
```

## 实现顺序

1. **`Op` + `Event` 枚举**：定义通信协议
2. **`Agent::spawn` + `submission_loop`**：跑通 channel 通信
3. **`run_turn` 骨架**：不含工具执行，只做模型调用 + 文本输出
4. **`Session`**：消息历史 + `build_request`
5. **工具执行**：在 `run_turn` 中接入 `ToolRegistry`
6. **审批流程**：`needs_approval` + `ApprovalResponse`
