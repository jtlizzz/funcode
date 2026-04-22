# ADR 0009: 以顶层 Item 作为内部 canonical 对话模型

**状态**: accepted
**日期**: 2026-04-26

## 背景

当前 `src/model.rs` 采用如下内部模型：

```rust
pub enum Message {
    System(String),
    User(String),
    Assistant(Vec<AssistantBlock>),
    Tool { call: ToolCall, result: ToolResult },
}

pub enum AssistantBlock {
    Text(String),
    ToolCall(ToolCall),
}
```

这套设计能较自然地兼容 OpenAI Chat Completions / Anthropic Messages 这类
"assistant message 内嵌 tool call block" 的协议，但它在 agent / session 层引入了两个问题：

1. **领域边界泄漏**  
   `agent.rs` 为了执行工具，不得不从 `Message::Assistant(Vec<AssistantBlock>)` 中提取
   `AssistantBlock::ToolCall`。这意味着 orchestrator 依赖了 provider-oriented 的消息内部结构。

2. **工具调用不是顶层领域对象**  
   在当前模型里，工具调用是 assistant message 的一部分；而工具结果则被单独建模为
   `Message::Tool`。这导致"发起调用"与"调用结果"不对称，不利于统一调度、存储、截断与回放。

3. **不利于多 provider 统一抽象**  
   Claude / OpenAI Chat Completions 倾向于使用 "message + content blocks" 表达工具调用；
   Codex / OpenAI Responses 则把 `FunctionCall` / `FunctionCallOutput` 作为顶层 item。
   如果 funcode 的内部模型继续绑定在 `AssistantBlock` 上，后续兼容 Codex/Responses 风格时，
   agent 与 session 层会持续受到 provider 形状影响。

4. **模型响应不是天然的“单条消息”**  
   一次响应在领域上可能同时包含：
   - assistant 文本
   - 一个或多个 tool call
   - 未来可能加入的 reasoning / web_search / image_generation 等输出

   因此，用单个 `Message` 作为响应终态抽象过窄。

参考项目的对应处理方式：

- **Claude Code**：`tool_use` 保留在 assistant content 内，`tool_result` 位于 user content。
- **Codex CLI**：`Message`、`FunctionCall`、`FunctionCallOutput` 都是并列的顶层 `ResponseItem`。
- **OpenCode**：内部持久化模型为 `message + parts`，对外统一投影为 AI SDK `ModelMessage[]`，
  provider 差异集中在 transform 层处理。

funcode 既希望兼容 Anthropic / Chat Completions，也希望内部模型更接近 Codex 的可扩展结构，
因此需要将内部 canonical model 从 "message-centric" 调整为 "item-centric"。

## 决策

### 1. 内部统一模型改为顶层 `Item`

将 funcode 的内部 canonical 对话模型定义为顶层 item 序列，而不是 assistant message block 树。

建议形态：

```rust
pub enum Item {
    Message(Message),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

pub enum Message {
    System(String),
    User(String),
    Assistant(String),
}
```

核心约束：

- `Message` 只表示 role-based text message。
- `ToolCall` / `ToolResult` 是与 `Message` 并列的领域对象。
- `Session` 历史保存 `Vec<Item>`。
- `ModelResponse` 返回 `Vec<Item>`，而不是单条 `Message`。
- `Agent` 只依赖顶层 `Item::ToolCall` 触发工具执行。

### 2. `AssistantBlock` 退出 domain 核心

`AssistantBlock` 不再作为 agent / session / model response 的核心领域类型存在。

它至多可以作为 provider adapter 内部的临时组装结构；更推荐直接在 adapter 中做
"provider payload <-> Vec<Item>" 的折叠/展开，不再保留通用 `AssistantBlock` 抽象。

### 3. Provider 适配层负责 fold / unfold

内部 canonical model 统一为 `Vec<Item>`；不同 provider 的请求/响应格式由 adapter 负责转换：

- **Anthropic / Claude Messages**
  - 入站：assistant text / tool_use block -> `Vec<Item>`
  - 出站：将连续的 `Assistant text + ToolCall*` 折叠为单条 assistant message content
  - `tool_result` 编码为 user message content

- **OpenAI Chat Completions**
  - 入站：assistant `content + tool_calls` -> `Vec<Item>`
  - 出站：将连续的 `Assistant text + ToolCall*` 折叠为单条 assistant message
  - `ToolResult` 编码为 tool role message

- **Codex / OpenAI Responses（未来）**
  - 可近乎直接映射到 `Item`

换言之：

- 外部协议是 provider-specific message shape
- 内部协议是 provider-neutral item stream

### 4. 参考 OpenCode：将 provider 怪癖集中到 transform 层

OpenCode 的经验表明，多 provider 兼容的关键不在于找到一种“完美统一的 message 结构”，
而在于明确分离三层职责：

1. **内部会话/持久化模型**  
   OpenCode 在内部保存 `MessageV2.Info + Part[]`，并不直接把 provider message 当作
   自己的核心域模型。

2. **统一请求投影层**  
   在发送前，将内部模型统一转换为 AI SDK `ModelMessage[]`。

3. **provider transform 层**  
   将 Anthropic / Mistral / OpenAI-compatible 等供应商的特殊限制，集中放在
   `ProviderTransform.message()` 一类函数中处理，例如：
   - 过滤空 content
   - 修正 tool call id 格式
   - 调整 assistant/tool result 顺序
   - 为 LiteLLM / 代理网关注入兼容字段

funcode 不复制 OpenCode 的 `message + parts` 结构，但吸收它的**分层策略**：

- 域模型：采用 Codex 风格 `Item`
- 请求投影：`Vec<Item>` -> provider messages
- provider 补丁：集中在 transform / adapter 层

这样可以避免：

- 让 `agent.rs` 直接理解 Anthropic 的 `tool_use` 排序约束
- 让 `session.rs` 直接理解 OpenAI Chat Completions 的 `tool` role 细节
- 在多个 provider 之间散落兼容分支

### 5. 流式完成态按 item 粒度输出

在当前实现中，流式路径不再用单个 `ItemsDone(ModelResponse)` 作为唯一权威终态，
而是拆成“观察事件 + 完成态事件 + 终止事件”三类：

- 观察事件：
  - `ResponseEvent::TextDelta(String)`
  - `ResponseEvent::ToolCallStart { ... }`
- 完成态领域事件：
  - `ResponseEvent::TextDone(String)`
  - `ResponseEvent::ToolCallReady { ... }`
- 响应终止事件：
  - `ResponseEvent::Completed { usage, finish_reason }`

这意味着：

- `Model` 仍然负责在 provider stream 内组装完整 assistant text / tool call
- `Agent` 收到完成态事件后即可把对应 `Item` 立即写入 `Session`
- `usage` / `finish_reason` 由最终 `Completed` 单独承载

`ModelResponse` 仍然保留，用于非流式 `send()` 返回完整响应：

```rust
pub struct ModelResponse {
    pub items: Vec<Item>,
    pub finish_reason: Option<String>,
    pub usage: Option<TokenUsage>,
}
```

## 重构草案

### 分层原则

本 ADR 接受后，funcode 的分层原则调整为：

- **Domain**
  - `Item`
  - `Message`
  - `ToolCall`
  - `ToolResult`
- **Provider Adapter**
  - `items_to_openai_messages()`
  - `openai_message_to_items()`
  - `items_to_anthropic_messages()`
  - `anthropic_message_to_items()`
- **Provider Transform**
  - provider id scrub / 顺序修正 / 空 content 过滤 / 兼容填充
- **Agent / Session**
  - 只消费 `Item`
  - 不依赖 `AssistantBlock`、provider role、provider chunk shape

### Step 1: 引入 `Item`，收窄 `Message`

在 `src/model.rs` 中：

- 新增 `Item`
- 将 `Message::Assistant(Vec<AssistantBlock>)` 改为 `Message::Assistant(String)`
- 删除 `Message::Tool`
- 保留 `ToolCall` / `ToolResult` 作为独立结构体

建议 API：

```rust
impl Item {
    pub fn as_message(&self) -> Option<&Message> { ... }
    pub fn as_tool_call(&self) -> Option<&ToolCall> { ... }
    pub fn as_tool_result(&self) -> Option<&ToolResult> { ... }
}
```

### Step 2: `Session` 改存 `Vec<Item>`

在 `src/session.rs` 中：

- `messages: Vec<Message>` -> `items: Vec<Item>`
- `push(Message)` -> `push(Item)`
- `messages()` -> `items()`
- `build_request()` 产出 `ModelRequest { items, tools, ... }`

`system_prompt` 仍然不进入历史，继续在 `build_request()` 时 prepend：

```rust
Item::Message(Message::System(...))
```

### Step 3: `ModelResponse` 改为 `Vec<Item>`

在 `src/model.rs` 中：

- `ModelRequest.messages` -> `ModelRequest.items`
- `ModelResponse.message` -> `ModelResponse.items`
- 非流式 `send()` 继续返回 `ModelResponse { items, usage, finish_reason }`
- 流式 `stream()` 改为输出：
  - `TextDelta`
  - `ToolCallStart`
  - `TextDone`
  - `ToolCallReady`
  - `Completed`

### Step 4: `agent.rs` 只依赖顶层 `ToolCall`

在 `src/agent.rs` 中：

- `run_turn()` 直接消费 `ResponseEvent`
- 收到 `TextDone` 时立即写入 `Item::Message(Message::Assistant(...))`
- 收到 `ToolCallReady` 时立即写入 `Item::ToolCall`
- 收到 `Completed` 时记录 `usage`
- 通过当前 turn 内收集到的 `ToolCall` 触发工具执行
- 工具结果回灌为 `Item::ToolResult`

示意：

```rust
match event {
    ResponseEvent::TextDone(text) => {
        session.push(Item::assistant(text));
    }
    ResponseEvent::ToolCallReady { id, name, arguments } => {
        let call = ToolCall::new(id, name, arguments);
        session.push(Item::tool_call(call.clone()));
        tool_calls.push(call);
    }
    ResponseEvent::Completed { usage, .. } => {
        if let Some(usage) = usage {
            session.record_usage(usage);
        }
    }
    _ => {}
}
```

### Step 5: Provider adapter 负责双向折叠

需要把当前：

- `message_to_openai()`
- `openai_message_to_chat()`

改造为：

- `items_to_openai_messages()`
- `openai_message_to_items()`

Anthropic adapter 也采用相同模式：

- `items_to_anthropic_messages()`
- `anthropic_message_to_items()`

这里的关键是 **fold / unfold 规则** 明确化：

1. 连续 `Assistant(String)` + `ToolCall*` 属于同一 assistant turn
2. `ToolResult` 总是单独编码成 provider 要求的结果消息
3. provider 无法严格表达的交错顺序（如 `text -> tool_call -> text`）允许有损投影

### Step 6: 引入独立 transform 层

在 adapter 之外新增 provider transform 辅助函数，专门处理供应商差异：

- `normalize_items_for_anthropic(...)`
- `normalize_items_for_openai_chat(...)`
- `sanitize_tool_call_id(...)`
- `repair_provider_message_sequence(...)`

这层不改变 canonical model，只改变**投影结果**，作用类似 OpenCode 的
`ProviderTransform.message()`。

### Step 7: 分阶段落地

建议按以下顺序实施，降低一次性重构风险：

1. 引入 `Item` 与 `ModelResponse.items`
2. `Session` 改存 `Vec<Item>`
3. `Agent` 改为只消费完成态 item 事件，并只依赖顶层 `Item::ToolCall`
4. OpenAI Chat adapter 改为 item fold/unfold
5. Anthropic adapter 改为 item fold/unfold
6. 删除 `AssistantBlock` 与 `Message::Tool`

## 备选方案

### 方案 A：保留 `AssistantBlock`，只给 `Message` 增加 `tool_calls()` helper

- **优点**：
  - 改动小
  - 兼容现有 OpenAI Chat Completions 实现最直接
- **缺点**：
  - `agent.rs` 虽然不直接 match `AssistantBlock`，但领域模型仍然是 provider-oriented
  - `ToolCall` 仍不是顶层领域对象
  - 不利于未来接入 Codex / Responses item 模型
- **为什么不选**：
  - 它只能缓解 API 使用体验，不能解决内部 canonical model 偏向某类 provider 的问题

### 方案 B：将 `ToolCall` / `ToolResult` 加入 `Message` 枚举

例如：

```rust
pub enum Message {
    System(String),
    User(String),
    Assistant(String),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}
```

- **优点**：
  - 实现比引入 `Item` 更直接
  - 也能把工具调用提升到顶层
- **缺点**：
  - `Message` 同时混合了 role message 与非 message item
  - 命名语义不准确，后续加入 reasoning / search call 会继续恶化
- **为什么不选**：
  - `Message` 应保持 role-text 语义；统一容器应由 `Item` 承担

### 方案 C：继续完全按 Claude 风格建模

- **优点**：
  - 对 Anthropic API 映射最自然
  - content block 顺序表达能力更强
- **缺点**：
  - 内部模型会持续依赖 assistant content block 结构
  - 与 Codex / Responses 的顶层 item 模型差异过大
  - 工具调度与持久化层需要长期理解 provider-specific message shape
- **为什么不选**：
  - funcode 的目标不是只兼容 Claude，而是建立 provider-neutral 的核心域模型

### 方案 D：完全复刻 OpenCode 的 `message + parts`

- **优点**：
  - 已在多 provider 场景验证可行
  - UI / 持久化 / 流式更新可以天然围绕 part 做增量存储
- **缺点**：
  - 与 funcode 当前架构和目标不完全一致
  - `ToolPart` 仍然是 message 的子结构，而不是 Codex 式顶层 item
  - 需要同时重构 message、storage、UI、stream processor 语义
- **为什么不选**：
  - funcode 此次目标是采用 Codex 风格的 canonical item model；OpenCode 更适合作为
    provider transform / 统一事件分层的参考，而不是直接复制其域模型

## 影响

### 正面影响

- `ToolCall` 成为顶层领域对象，agent 工具调度更直接。
- session / truncate / replay / persistence 可以统一面向 `Item` 处理。
- 与 Codex / Responses 风格对齐，未来扩展 reasoning / web_search / image_generation 更自然。
- provider 兼容逻辑被收敛到 adapter 层，领域边界更清晰。
- 可借鉴 OpenCode，将 provider-specific 修补集中到 transform 层，而不是散落在业务逻辑里。

### 负面影响

- `src/model.rs`、`src/session.rs`、`src/agent.rs` 的核心类型签名都会变更。
- request/response adapter 需要从“单条 message 转换”升级为“item 序列折叠/展开”。
- 现有测试中大量 `Message::Assistant(_)` / `AssistantBlock` 断言需要重写。

### 风险

- **风险 1：迁移跨度较大**  
  需要同步修改 Model / Session / Agent / tests。  
  **缓解**：按 `Item` 引入 -> `ModelResponse.items` -> `Session.items` -> provider adapter 的顺序分步迁移。

- **风险 2：Chat Completions 的顺序表达能力较弱**  
  内部 item 流可能比 provider 能表达的顺序更细。  
  **缓解**：明确允许 adapter 做有损折叠，并将 canonical model 设计优先级置于 provider 限制之上。

- **风险 3：ADR-0001 与本决策存在冲突**  
  ADR-0001 当前接受了 `Assistant(Vec<AssistantBlock>)` 方案。  
  **缓解**：本 ADR 被接受后，应将 ADR-0001 标记为 superseded，或在其顶部增加 superseded 说明。

## 参考实现

- Claude Code
  - `参考: /home/acer/project/node_project/claude-code/src/services/api/claude.ts`
  - `参考: /home/acer/project/node_project/claude-code/src/utils/messages.ts`
- Codex CLI
  - `参考: /home/acer/project/rust_project/codex-main/codex-rs/protocol/src/models.rs`
  - `参考: /home/acer/project/rust_project/codex-main/codex-rs/core/src/tools/router.rs`
- OpenCode
  - `参考: /home/acer/project/node_project/opencode/packages/opencode/src/session/message-v2.ts`
  - `参考: /home/acer/project/node_project/opencode/packages/opencode/src/session/processor.ts`
  - `参考: /home/acer/project/node_project/opencode/packages/opencode/src/provider/transform.ts`
