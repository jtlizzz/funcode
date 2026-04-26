//! Agent 编排核心模块。
//!
//! 实现 Agent 主循环：接收用户操作 → 流式调用模型 → 执行工具 → 继续或停止。
//! 通过 [`Bus`] 事件总线向外部推送实时事件。
//!
//! # 核心循环
//!
//! ```text
//! loop {
//!     request = session.build_request(registry.specs())
//!     stream = model.stream(request)
//!     (blocks, usage) = consume_stream(stream)
//!     session.push(Assistant(blocks))
//!     if 有 ToolCall {
//!         results = execute_tools(tool_calls)
//!         session.push(Tool { call, result })
//!         continue
//!     } else {
//!         break
//!     }
//! }
//! ```
//!
//! 参考:
//! - Claude Code `src/query.ts` — `queryLoop()` AsyncGenerator
//! - Codex CLI `codex-rs/core/src/codex_thread.rs` — `run_turn()`
//! - OpenCode `session/prompt.ts` — `runLoop()`

use tokio::sync::watch;

use crate::bus::{Bus, Event};
use crate::model::{
    AssistantBlock, Message, Model, ModelError, ResponseEvent, ResponseStream, ToolCall,
    TokenUsage, ToolResult,
};
use crate::session::Session;
use crate::tools::ToolRegistry;

// ==================== Op 枚举 ====================

/// Agent 操作，外部通过 [`Agent::submit`] 提交。
///
/// 参考 Codex CLI 的统一 `Op` 枚举模式：所有操作通过同一个入口进入。
/// Claude Code 使用分散机制（`AbortController` + `setModel()`），
/// funcode 选择 Codex 的统一模式，但 Phase 1 只保留两个变体。
pub enum Op {
    /// 用户发送文本消息，开始一轮新的对话。
    UserTurn(String),
    /// 用户中断当前正在进行的模型生成或工具执行。
    ///
    /// 参考 Claude Code `QueryEngine.interrupt()`:
    /// `this.abortController.abort()`
    /// 以及 Codex CLI 的 `CancellationToken.cancel()`。
    Interrupt,
}

// ==================== Agent ====================

/// Agent 核心结构，串联 Model / Session / ToolRegistry / Bus。
pub struct Agent {
    model: Model,
    session: Session,
    registry: ToolRegistry,
    bus: Bus,
    max_turns: usize,
    /// 中断信号：`true` 时 run_turn 在下一次循环检查时停止。
    ///
    /// 参考 Claude Code 的 `AbortController` 和 Codex 的 `CancellationToken`。
    /// Phase 1 用 `watch::Sender<bool>` 实现，轻量且可异步检查。
    interrupt_tx: watch::Sender<bool>,
    interrupt_rx: watch::Receiver<bool>,
}

impl Agent {
    /// 创建一个新的 Agent。
    ///
    /// `max_turns` 为单次 `submit` 中允许的最大循环次数（防止无限循环）。
    /// 参考 Claude Code `query.ts` 的 `maxTurns` 参数。
    pub fn new(
        model: Model,
        session: Session,
        registry: ToolRegistry,
        bus: Bus,
        max_turns: usize,
    ) -> Self {
        let (interrupt_tx, interrupt_rx) = watch::channel(false);
        Self {
            model,
            session,
            registry,
            bus,
            max_turns,
            interrupt_tx,
            interrupt_rx,
        }
    }

    /// 返回事件总线的只读引用，供外部 `subscribe`。
    pub fn bus(&self) -> &Bus {
        &self.bus
    }

    /// 返回 session 的只读引用。
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// 提交一个操作。
    ///
    /// 这是外部调用 Agent 的唯一入口：
    /// - `Op::UserTurn(text)` → 开始一轮对话
    /// - `Op::Interrupt` → 中断当前生成
    pub async fn submit(&mut self, op: Op) {
        match op {
            Op::UserTurn(text) => {
                self.session.push(Message::user(text));
                self.run_turn().await;
            }
            Op::Interrupt => {
                // 参考 Claude Code: `this.abortController.abort()`
                let _ = self.interrupt_tx.send(true);
            }
        }
    }

    // ==================== 核心循环 ====================

    /// Agent 主循环。
    ///
    /// 循环执行：构建请求 → 流式调用模型 → 消费事件 → 执行工具 → 继续/停止。
    ///
    /// 参考 Claude Code `query.ts` 的 `while (true)` 循环:
    /// ```ignore
    /// while (true) {
    ///     for await (const message of deps.callModel({ messages, tools })) {
    ///         if (tool_use blocks found) needsFollowUp = true
    ///     }
    ///     if (needsFollowUp) { execute tools; state = next; continue }
    ///     else { return { reason: 'completed' } }
    /// }
    /// ```
    async fn run_turn(&mut self) {
        // 重置中断信号
        let _ = self.interrupt_tx.send(false);

        self.bus.publish(Event::TurnStarted);

        for turn in 0..self.max_turns {
            // 检查中断
            if *self.interrupt_rx.borrow() {
                self.bus.publish(Event::Error("interrupted".to_string()));
                return;
            }

            // 截断检查
            self.session.truncate_to_budget();

            // 构建请求
            let tools = self.registry.specs();
            let request = self.session.build_request(&tools);

            // 流式调用模型
            let stream = match self.model.stream(request).await {
                Ok(s) => s,
                Err(err) => {
                    self.bus.publish(Event::Error(err.to_string()));
                    return;
                }
            };

            // 消费流式响应
            let (blocks, usage) = self.consume_stream(stream).await;

            // 记录 assistant 消息和 token 使用
            self.session.push(Message::assistant(blocks.clone()));
            if let Some(u) = usage {
                self.session.record_usage(u);
            }

            // 提取工具调用
            let tool_calls: Vec<&ToolCall> = blocks
                .iter()
                .filter_map(|b| match b {
                    AssistantBlock::ToolCall(tc) => Some(tc),
                    _ => None,
                })
                .collect();

            if tool_calls.is_empty() {
                // 无工具调用 → 正常完成
                // 参考 Claude Code: `return { reason: 'completed', turnCount }`
                // 参考 Codex CLI: `if !needs_follow_up { break }`
                let final_usage = usage;
                self.bus.publish(Event::TurnComplete {
                    usage: final_usage,
                });
                return;
            }

            // 执行工具
            let results = self.execute_tools(&tool_calls).await;

            // 将工具结果推入 session
            for (call, result) in results {
                self.session.push(Message::tool(call, result));
            }

            // 继续下一轮（模型将看到工具结果并决定下一步）
            // 参考 Claude Code:
            // `state = { messages: [...messages, ...toolResults] }`
            let _ = turn; // turn 仅用于 max_turns 计数
        }

        // 超过 max_turns
        // 参考 Claude Code: `return { reason: 'max_turns', turnCount }`
        self.bus.publish(Event::Error(format!(
            "max turns reached ({})",
            self.max_turns
        )));
    }

    // ==================== 流式消费 ====================

    /// 消费模型流式响应，实时推送事件到 Bus。
    ///
    /// 返回收集到的完整 `AssistantBlock` 列表和 token usage。
    ///
    /// 参考:
    /// - Claude Code `query.ts`: `for await (const message of deps.callModel())`
    /// - Codex CLI `try_run_sampling_request`: 逐 SSE 事件 match
    /// - OpenCode `processor.ts` `handleEvent`: 按 type 分发
    async fn consume_stream(
        &mut self,
        mut stream: ResponseStream,
    ) -> (Vec<AssistantBlock>, Option<TokenUsage>) {
        use futures_util::StreamExt;

        let mut text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut usage = None;

        while let Some(result) = stream.next().await {
            // 检查中断
            if *self.interrupt_rx.borrow() {
                break;
            }

            let event = match result {
                Ok(e) => e,
                Err(err) => {
                    self.bus.publish(Event::Error(err.to_string()));
                    break;
                }
            };

            match event {
                ResponseEvent::TextDelta(delta) => {
                    // 参考 Codex CLI: `emit_streamed_assistant_text_delta()`
                    text.push_str(&delta);
                    self.bus.publish(Event::TextDelta(delta));
                }
                ResponseEvent::ToolCallStart { id, name } => {
                    // UI 提示：模型开始提及一个工具调用
                    // 参考 Claude Code `StreamingToolExecutor.addTool()`
                    self.bus.publish(Event::ToolCallBegin {
                        id: id.clone(),
                        name: name.clone(),
                    });
                }
                ResponseEvent::ToolCallReady {
                    id,
                    name,
                    arguments,
                } => {
                    // 参数接收完毕，工具准备好执行
                    tool_calls.push(ToolCall::new(id, name, arguments));
                }
                ResponseEvent::MessageDone(response) => {
                    // 整条消息完成
                    // 参考 Claude Code: 从 response 提取 usage 和 finish_reason
                    if let Some(u) = response.usage {
                        usage = Some(u);
                    }
                }
            }
        }

        // 组装 blocks
        let mut blocks = Vec::new();
        if !text.is_empty() {
            blocks.push(AssistantBlock::text(&text));
            self.bus.publish(Event::TextDone(text));
        }
        for tc in tool_calls {
            blocks.push(AssistantBlock::ToolCall(tc));
        }

        (blocks, usage)
    }

    // ==================== 工具执行 ====================

    /// 执行工具调用列表，返回 (ToolCall, ToolResult) 对。
    ///
    /// Phase 1 串行执行。Phase 2 加入 `is_concurrency_safe` 分区并行。
    ///
    /// 参考:
    /// - Claude Code `toolOrchestration.ts`: `runToolsSerially()`
    /// - Codex CLI: `FuturesOrdered` 并行执行
    async fn execute_tools(&self, calls: &[&ToolCall]) -> Vec<(ToolCall, ToolResult)> {
        let mut results = Vec::with_capacity(calls.len());

        for call in calls {
            let result = self
                .registry
                .execute(&call.id, &call.name, &call.arguments)
                .await;

            self.bus.publish(Event::ToolCallEnd {
                id: call.id.clone(),
                name: call.name.clone(),
                output: result.content.clone(),
                is_error: result.is_error,
            });

            results.push(((*call).clone(), result));
        }

        results
    }
}

// ==================== Tests ====================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelProvider, ModelRequest, ModelResponse};
    use crate::tools::Tool;
    use async_trait::async_trait;
    use serde_json::json;

    // === Mock Provider ===

    /// Mock provider: 返回纯文本响应，无工具调用。
    struct TextProvider {
        response: String,
    }

    #[async_trait]
    impl ModelProvider for TextProvider {
        async fn send(
            &self,
            _model: &str,
            _request: ModelRequest,
        ) -> Result<ModelResponse, ModelError> {
            Ok(ModelResponse {
                message: Message::assistant_text(&self.response),
                finish_reason: Some("stop".to_string()),
                usage: Some(TokenUsage {
                    input_tokens: Some(10),
                    output_tokens: Some(5),
                    total_tokens: Some(15),
                }),
            })
        }

        async fn stream(
            &self,
            _model: &str,
            _request: ModelRequest,
        ) -> Result<ResponseStream, ModelError> {
            let (tx, rx) = tokio::sync::mpsc::channel(32);

            let text = self.response.clone();
            tokio::spawn(async move {
                let _ = tx.send(Ok(ResponseEvent::TextDelta(text.clone()))).await;
                let _ = tx
                    .send(Ok(ResponseEvent::MessageDone(ModelResponse {
                        message: Message::assistant_text(&text),
                        finish_reason: Some("stop".to_string()),
                        usage: Some(TokenUsage {
                            input_tokens: Some(10),
                            output_tokens: Some(5),
                            total_tokens: Some(15),
                        }),
                    })))
                    .await;
            });

            Ok(ResponseStream::new(rx))
        }
    }

    /// Mock provider: 总是返回一个工具调用。
    struct ToolCallProvider;

    #[async_trait]
    impl ModelProvider for ToolCallProvider {
        async fn send(
            &self,
            _model: &str,
            _request: ModelRequest,
        ) -> Result<ModelResponse, ModelError> {
            Ok(ModelResponse {
                message: Message::assistant_text("done"),
                finish_reason: Some("stop".to_string()),
                usage: None,
            })
        }

        async fn stream(
            &self,
            _model: &str,
            _request: ModelRequest,
        ) -> Result<ResponseStream, ModelError> {
            let (tx, rx) = tokio::sync::mpsc::channel(32);
            tokio::spawn(async move {
                let _ = tx
                    .send(Ok(ResponseEvent::ToolCallStart {
                        id: "call_1".to_string(),
                        name: "echo".to_string(),
                    }))
                    .await;
                let _ = tx
                    .send(Ok(ResponseEvent::ToolCallReady {
                        id: "call_1".to_string(),
                        name: "echo".to_string(),
                        arguments: r#"{"message":"hello"}"#.to_string(),
                    }))
                    .await;
                let _ = tx
                    .send(Ok(ResponseEvent::MessageDone(ModelResponse {
                        message: Message::assistant(vec![AssistantBlock::tool_call(
                            "call_1",
                            "echo",
                            r#"{"message":"hello"}"#,
                        )]),
                        finish_reason: Some("tool_calls".to_string()),
                        usage: Some(TokenUsage {
                            input_tokens: Some(50),
                            output_tokens: Some(20),
                            total_tokens: Some(70),
                        }),
                    })))
                    .await;
            });
            Ok(ResponseStream::new(rx))
        }
    }

    // === Helpers ===

    fn text_agent(response: &str) -> Agent {
        let model = Model::new(
            Box::new(TextProvider {
                response: response.to_string(),
            }),
            "test-model",
        )
        .unwrap();
        let session = Session::new("You are helpful.", 100_000);
        let registry = ToolRegistry::new();
        let bus = Bus::new(64);
        Agent::new(model, session, registry, bus, 10)
    }

    /// Echo tool for testing.
    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes arguments"
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {"message": {"type": "string"}}})
        }
        async fn execute(&self, args: &str) -> Result<String, crate::tools::ToolError> {
            Ok(args.to_string())
        }
    }

    async fn collect_events(sub: &mut crate::bus::Subscriber, max: usize) -> Vec<Event> {
        let mut events = Vec::with_capacity(max);
        for _ in 0..max {
            match tokio::time::timeout(std::time::Duration::from_millis(500), sub.recv()).await {
                Ok(Some(crate::bus::ReceiveResult::Event(e))) => events.push(e),
                _ => break,
            }
        }
        events
    }

    // === Tests ===

    #[tokio::test]
    async fn text_only_turn_completes() {
        let mut agent = text_agent("Hello world");
        let mut sub = agent.bus().subscribe();

        agent.submit(Op::UserTurn("hi".to_string())).await;

        // 应该收到: TurnStarted → TextDelta → TextDone → TurnComplete
        let events = collect_events(&mut sub, 4).await;
        assert!(events.contains(&Event::TurnStarted));
        assert!(events.contains(&Event::TextDelta("Hello world".to_string())));
        assert!(events.contains(&Event::TextDone("Hello world".to_string())));
        assert!(matches!(
            &events[3],
            Event::TurnComplete { usage: Some(_) }
        ));

        // session 应该有 2 条消息: user + assistant
        assert_eq!(agent.session().len(), 2);
        assert_eq!(agent.session().total_tokens(), 15);
    }

    #[tokio::test]
    async fn interrupt_signal_works() {
        let mut agent = text_agent("response");

        // 中断信号默认为 false
        assert!(!*agent.interrupt_rx.borrow());

        // 发送中断
        agent.submit(Op::Interrupt).await;
        assert!(*agent.interrupt_rx.borrow());

        // 正常 submit 会重置中断信号并正常完成
        let mut sub = agent.bus().subscribe();
        agent.submit(Op::UserTurn("hi".to_string())).await;

        // 重置后应该正常完成
        assert!(!*agent.interrupt_rx.borrow());
        let events = collect_events(&mut sub, 4).await;
        assert!(events.contains(&Event::TurnStarted));
        assert!(events.iter().any(|e| matches!(e, Event::TurnComplete { .. })));
    }

    #[tokio::test]
    async fn max_turns_limits_loop() {
        let model = Model::new(Box::new(ToolCallProvider), "test-model").unwrap();
        let session = Session::new("system", 100_000);
        let registry = {
            let mut r = ToolRegistry::new();
            r.register(Box::new(EchoTool));
            r
        };
        let bus = Bus::new(64);
        let mut agent = Agent::new(model, session, registry, bus, 2);

        agent.submit(Op::UserTurn("use tool".to_string())).await;

        // max_turns=2，ToolCallProvider 每次都返回工具调用
        // 应该在 2 轮后因 max_turns 停止
        let messages = agent.session().messages();
        assert!(messages.len() >= 3);
    }

    #[tokio::test]
    async fn tool_execution_and_result_pushed() {
        let model = Model::new(Box::new(ToolCallProvider), "test-model").unwrap();
        let session = Session::new("system", 100_000);
        let registry = {
            let mut r = ToolRegistry::new();
            r.register(Box::new(EchoTool));
            r
        };
        let bus = Bus::new(64);
        let mut agent = Agent::new(model, session, registry, bus, 1);

        agent.submit(Op::UserTurn("use echo".to_string())).await;

        let msgs = agent.session().messages();
        // user + assistant(tool_call) + tool_result
        assert_eq!(msgs.len(), 3);

        match &msgs[2] {
            Message::Tool { call, result } => {
                assert_eq!(call.name, "echo");
                assert!(!result.is_error);
                assert!(result.content.contains("hello"));
            }
            _ => panic!("expected Tool message"),
        }
    }

    #[tokio::test]
    async fn bus_events_for_tool_execution() {
        let model = Model::new(Box::new(ToolCallProvider), "test-model").unwrap();
        let session = Session::new("system", 100_000);
        let registry = {
            let mut r = ToolRegistry::new();
            r.register(Box::new(EchoTool));
            r
        };
        let bus = Bus::new(64);
        let mut agent = Agent::new(model, session, registry, bus, 1);
        let mut sub = agent.bus().subscribe();

        agent.submit(Op::UserTurn("go".to_string())).await;

        let events = collect_events(&mut sub, 5).await;
        assert!(events.contains(&Event::TurnStarted));
        assert!(events.contains(&Event::ToolCallBegin {
            id: "call_1".to_string(),
            name: "echo".to_string(),
        }));
        assert!(events.iter().any(|e| matches!(
            e,
            Event::ToolCallEnd { name, is_error: false, .. } if name == "echo"
        )));
    }
}
