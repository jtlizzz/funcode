# ADR 0008: CancellationToken + 流式组装职责统一

**状态**: 待实施
**日期**: 2026-04-26

## 背景

当前 agent.rs 和 model.rs 存在两个问题：

1. **双重累积**：model.rs 的 OpenAIProvider::stream 内部累积 text + PartialToolCall 组装完整 blocks，通过 MessageDone 事件发出；agent.rs 的 consume_stream 又从 TextDelta/ToolCallReady 重新累积了一遍，MessageDone 携带的完整消息被忽略。
2. **中断信号无法传递到 model 层**：用户中断后，agent 的 consume_stream break 退出，ResponseStream 被 drop，spawned task 只在下一次 tx_event.send() 失败时才发现。中间有窗口期继续消耗 SSE 数据。

## 决策

### 1. 引入 `tokio_util::sync::CancellationToken`

替换当前的 `watch::Sender/Receiver<bool>` 中断机制。

**理由**：
- CancellationToken 是 Rust 异步生态取消的事实标准（Codex CLI 直接使用）
- `child_token()` 天然级联：父取消 → 子全部取消
- `.or_cancel(&token)` 直接绑定 Future，无需手动检查
- 语义清晰："取消操作" 而非 "广播一个布尔值"

**参考**：
- Codex CLI `codex-rs/core/src/codex.rs:7052-7059` — `.or_cancel(&cancellation_token)` 在每个 stream.next() 上绑定
- Codex CLI `codex-rs/core/src/tasks/mod.rs:169-200` — CancellationToken 创建 + child_token 传递
- Codex CLI `codex-rs/core/src/tasks/mod.rs:404-425` — cancel() 触发后优雅等待 + 超时 abort
- Claude Code `src/query.ts:707` — AbortSignal 从 agent 层传入 API 层

### 2. Model 层负责消息组装，Agent 层只消费

Model 层（ResponseStream）作为有状态的领域对象：一边推送增量事件（TextDelta、ToolCallStart），一边内部自动累积完整 blocks。最终通过 MessageDone 提供权威结果。

Agent 层 consume_stream 的职责简化为：
- 从增量事件推 Bus（给 UI 实时渲染）
- 从 MessageDone 取最终 blocks（给 session 存储）
- 不做任何重复累积

## 改造计划

### Step 1: Cargo.toml 添加依赖

```toml
tokio-util = { version = "0.7", features = ["rt"] }
```

### Step 2: model.rs — ResponseStream 持有 CancellationToken

```rust
pub struct ResponseStream {
    rx_event: mpsc::Receiver<Result<ResponseEvent, ModelError>>,
    cancel: CancellationToken,
}

impl Stream for ResponseStream {
    type Item = Result<ResponseEvent, ModelError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.cancel.is_cancelled() {
            return Poll::Ready(None);
        }
        self.rx_event.poll_recv(cx)
    }
}
```

### Step 3: model.rs — ModelProvider::stream() 签名加 cancel

```rust
#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn send(&self, model: &str, request: ModelRequest) -> Result<ModelResponse, ModelError>;
    async fn stream(
        &self,
        model: &str,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ResponseStream, ModelError>;
}

impl Model {
    pub async fn stream(&self, request: ModelRequest, cancel: CancellationToken) -> Result<ResponseStream, ModelError> {
        self.provider.stream(&self.model, request, cancel).await
    }
}
```

### Step 4: model.rs — OpenAIProvider spawned task 检查 cancel

```rust
let task_cancel = cancel.clone();
tokio::spawn(async move {
    while let Some(result) = stream.next().await {
        if task_cancel.is_cancelled() { return; }
        // ... 原有 chunk 处理逻辑不变
    }
    // ... 原有 ToolCallReady + MessageDone 发射逻辑不变
});
Ok(ResponseStream { rx_event, cancel })
```

### Step 5: agent.rs — CancellationToken 替换 watch channel

```rust
pub struct Agent {
    model: Model,
    session: Session,
    registry: ToolRegistry,
    bus: Bus,
    max_turns: usize,
    cancel: CancellationToken,     // 替换 interrupt_tx/rx
}

// Op::Interrupt 时：
self.cancel.cancel();

// Op::UserTurn 时重置：
self.cancel = CancellationToken::new();
```

### Step 6: agent.rs — consume_stream 简化

```rust
async fn consume_stream(
    &mut self,
    mut stream: ResponseStream,
) -> (Vec<AssistantBlock>, Option<TokenUsage>) {
    let mut final_response = None;

    loop {
        let result = match stream.next().or_cancel(&self.cancel).await {
            Ok(Some(r)) => r,
            Ok(None) => break,
            Err(_) => {
                self.bus.publish(Event::Error("interrupted".into()));
                break;
            }
        };

        match result {
            Ok(ResponseEvent::TextDelta(delta)) => {
                self.bus.publish(Event::TextDelta(delta));
            }
            Ok(ResponseEvent::ToolCallBegin { id, name }) => {
                self.bus.publish(Event::ToolCallBegin { id, name });
            }
            Ok(ResponseEvent::ToolCallReady { .. }) => {
                // Phase 2: 在这里 spawn 工具执行
            }
            Ok(ResponseEvent::MessageDone(response)) => {
                final_response = Some(response);
            }
            Err(err) => {
                self.bus.publish(Event::Error(err.to_string()));
                break;
            }
        }
    }

    match final_response {
        Some(resp) => {
            let blocks = match resp.message {
                Message::Assistant(b) => b,
                _ => vec![],
            };
            if let Some(text) = blocks.iter().find_map(|b| match b {
                AssistantBlock::Text(t) => Some(t.clone()),
                _ => None,
            }) {
                self.bus.publish(Event::TextDone(text));
            }
            (blocks, resp.usage)
        }
        None => (vec![], None),
    }
}
```

### Step 7: 测试适配

- 所有 mock provider 的 `stream()` 签名加 `CancellationToken` 参数
- `interrupt_signal_works` 测试改用 `cancel.cancel()` 验证
- 新增测试：验证流式中途取消时 spawned task 停止

## 影响范围

| 文件 | 改动 |
|------|------|
| `Cargo.toml` | 添加 `tokio-util` |
| `model.rs` | ResponseStream 加 cancel 字段；Stream impl 检查取消；ModelProvider::stream 签名变更；OpenAIProvider spawned task 检查取消 |
| `agent.rs` | 去掉 watch channel 换 CancellationToken；consume_stream 去掉重复累积，用 .or_cancel()，从 MessageDone 取结果 |
| `bus.rs` | 无变更 |
| `session.rs` | 无变更 |
| `tools.rs` | 无变更 |

## 未来兼容

- Phase 2 流式工具执行：ToolCallReady 在 spawned task 内参数接收完毕时立即发射，agent 收到即可 spawn 执行
- `child_token()` 支持工具执行级别的独立取消
