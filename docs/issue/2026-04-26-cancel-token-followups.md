# 取消令牌后续事项

日期: 2026-04-26
相关 ADR: `docs/adr/0008-cancel-token-stream-assembly.md`

## 状态

本文记录在 `CancellationToken` 重构过程中识别出的两个取消相关问题，
以及它们在当前实现中的解决方式。

## 问题 1：没有权威终止事件的 EOF 被误判为成功

- 严重程度: 高
- 状态: 已解决

### 原始问题

旧的 `consume_stream()` 路径可能会把 EOF 当作正常的流结束处理，
即使 provider 从未发出权威的响应终止事件。这样会导致一个不完整的 turn
看起来像是成功完成了。

### 解决方案

现在 stream loop 已内联到 `Agent::run_turn()` 中，并且只有在收到以下事件后，
才认为当前 turn 成功完成：

```rust
ResponseEvent::Completed { usage, finish_reason }
```

在 `Completed` 之前，agent 也可能收到权威的 item 完成事件，
例如 `TextDone` 和 `ToolCallReady`。

如果 stream 在 `Completed` 之前结束，agent 现在会报告协议/运行时错误，
而不是发布 `TurnComplete`。

## 问题 2：晚到取消可能抢先遮蔽已经缓冲的最终响应

- 严重程度: 中
- 状态: 已解决

### 原始问题

早期设计曾考虑在 `ResponseStream::poll_next()` 中检查取消状态。
如果取消刚好发生在 provider 产出权威事件之后，这会让 consumer 侧的取消
遮蔽已经缓冲好的权威事件。

### 解决方案

最终设计**不**让 `ResponseStream` 直接检查 cancellation token。取而代之：

- provider 侧的 stream task 观察 `CancellationToken`
- provider 发出显式的 `ResponseEvent::Cancelled`
- `ResponseStream` 只负责从事件通道中取出事件

这样既能保证 agent 仍然可以观察到已经缓冲的终止事件，
也能让上游工作及时停止。

## 剩余后续事项

当前取消机制只会中断模型流。它还**不会**取消已经开始运行的工具执行。
这项后续工作有意保留在本次变更范围之外。
