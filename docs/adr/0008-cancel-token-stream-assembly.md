# ADR 0008: CancellationToken + 流式组装职责统一

**状态**: accepted
**日期**: 2026-04-26

## 背景

在引入顶层 `Item` 作为 canonical 对话模型之前，`agent.rs` 和 `model.rs` 曾经同时承担部分
流式组装职责：

1. `model.rs` 在 provider stream 内累积完整响应。
2. `agent.rs` 再次从 `TextDelta` / `ToolCallReady` 重新拼装最终结果。
3. 用户中断通过 `watch::Sender<bool>` 传播，只能让 agent 侧尽快退出，不能明确传达到 model
   层的流式任务。

这会带来两个问题：

- **职责重复**：完整响应到底以哪一层的累积结果为准并不清晰。
- **取消语义含糊**：中断更像“本地 break”，而不是明确的 turn 级取消信号。

在采纳 ADR-0009 的 `Vec<Item>` 内部模型后，这两个问题更需要收口：

- `Model` 应该产出权威的“完成态 item 事件”和终态 `Completed`
- `Agent` 应只消费流式 domain event，不再从 delta 重复累积完整消息

## 决策

### 1. 使用 `tokio_util::sync::CancellationToken`

`Agent` 的 turn 级中断机制统一采用 `CancellationToken`，替换之前的
`watch::Sender/Receiver<bool>`。

理由：

- `CancellationToken` 是 Rust 异步生态中更自然的取消原语
- 语义明确：这是“取消当前 turn”，而不是“轮询一个布尔值”
- 能直接传入 model/provider 层，让流式任务感知取消

### 2. provider 负责把取消转成显式 stream event

`ResponseStream` 自身只做事件通道，不再在 `poll_next()` 中抢先检查取消。

取消由 provider 的流式任务处理：

- 上游收到 `CancellationToken` 后停止继续消费 SSE
- 向 `ResponseStream` 发出显式 `ResponseEvent::Cancelled`

这样可以避免 consumer 侧为了响应取消而丢弃已经入队的权威完成态事件。

### 3. `TextDone` / `ToolCallReady` / `Completed` 是流式权威完成态，`Agent` 只消费

`Agent` 的 turn 主循环直接内联 stream event loop：

- `TextDelta` / `ToolCallStart` 只做观察事件转发
- `Cancelled` 结束当前 turn，并通过 `Bus` 发出 `TurnInterrupted`
- `TextDone` 表示一条完整 assistant 文本已经完成，可直接写入 `Session`
- `ToolCallReady` 表示一条完整 tool call 已经完成，可直接写入 `Session`
- `Completed` 提供本次模型响应的 `usage` 与 `finish_reason`

`Agent` 不再保留独立的 `consume_stream()` 聚合器，也不再从增量事件重新拼装最终响应。
消息和工具调用的组装仍由 `Model` 在 provider stream 内部完成；`Agent` 只消费完成后的领域事件。

### 4. 中断不是错误

用户取消当前 turn 属于预期控制流，不属于错误。

因此在 `Bus` 中增加独立事件：

```rust
Event::TurnInterrupted
```

而不是继续复用：

```rust
Event::Error("interrupted".to_string())
```

## 实施结果

本 ADR 当前对应的实现包括：

- `Cargo.toml`
  - 添加 `tokio-util`
- `src/agent.rs`
  - `watch` 中断机制替换为 `CancellationToken`
  - stream event loop 内联到 `run_turn()`
  - `Cancelled` -> `Event::TurnInterrupted`
  - `TextDone` / `ToolCallReady` 到达时立即把完成 item 写入 `Session`
  - `Completed` 到达后记录 `usage` 并判定本轮模型流正常结束
- `src/model.rs`
  - `ModelProvider::stream()` 签名接收 `CancellationToken`
  - provider 侧在取消时发出 `ResponseEvent::Cancelled`
  - provider 侧产出完成态事件：
    - `ResponseEvent::TextDone(String)`
    - `ResponseEvent::ToolCallReady { ... }`
    - `ResponseEvent::Completed { usage, finish_reason }`
- `src/bus.rs`
  - 新增 `Event::TurnInterrupted`

## 影响

### 优点

- `Agent` / `Model` 的职责边界更清晰
- turn 中断语义更明确
- 避免 consumer 侧取消抢先丢弃已缓冲的权威完成态事件
- 为后续 `ToolCallReady` 驱动的流式工具调度打下基础

### 当前不涵盖的范围

本 ADR 当前只保证中断流式模型响应。

工具执行取消仍未实现：

- `Tool::execute()` 还没有接收 `CancellationToken`
- `Agent::Interrupt` 还不会中止已经开始运行的工具

这部分留待后续迭代处理。

## 参考

- Codex CLI `codex-rs/core/src/codex.rs`
- Codex CLI `codex-rs/core/src/tools/parallel.rs`
- Claude Code `src/query.ts`
