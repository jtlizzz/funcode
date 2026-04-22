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
//!     (tool_calls, usage) = inline stream event loop
//!     if 有 ToolCall {
//!         results = execute_tools(tool_calls)
//!         session.push(ToolResult)
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

use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::bus::{Bus, Event};
use crate::model::{
    Item, Message, Model, ModelError, ResponseEvent, ResponseStream, TokenUsage, ToolCall,
    ToolResult,
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
    /// 用户中断当前正在进行的模型生成。
    ///
    /// 当前只保证中断流式模型响应；工具执行取消仍留待后续实现。
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
    /// 中断信号：取消时 run_turn 立即停止流式消费。
    ///
    /// 参考 Codex CLI 的 `CancellationToken`。
    cancel: CancellationToken,
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
        Self {
            model,
            session,
            registry,
            bus,
            max_turns,
            cancel: CancellationToken::new(),
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
                self.session.push(Item::user(text));
                self.run_turn().await;
            }
            Op::Interrupt => {
                // 参考 Codex CLI: `CancellationToken.cancel()`
                self.cancel.cancel();
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
    ///     for await (const item of deps.callModel({ messages, tools })) {
    ///         if (tool_calls found) needsFollowUp = true
    ///     }
    ///     if (needsFollowUp) { execute tools; state = next; continue }
    ///     else { return { reason: 'completed' } }
    /// }
    /// ```
    async fn run_turn(&mut self) {
        // 重置中断信号：每次新 turn 使用新的 token
        self.cancel = CancellationToken::new();

        self.bus.publish(Event::TurnStarted);

        for turn in 0..self.max_turns {
            // 检查中断
            if self.cancel.is_cancelled() {
                self.bus.publish(Event::TurnInterrupted);
                return;
            }

            // 截断检查
            self.session.truncate_to_budget();

            // 构建请求
            let tools = self.registry.specs();
            let request = self.session.build_request(&tools);

            // 流式调用模型（传入 cancel token）
            let cancel = self.cancel.clone();
            let mut stream = match self.model.stream(request, cancel).await {
                Ok(s) => s,
                Err(err) => {
                    self.bus.publish(Event::Error(err.to_string()));
                    return;
                }
            };

            // 流式消费响应；权威完成态由 TextDone / ToolCallReady / Completed 表示。
            let mut tool_calls = Vec::new();
            let usage = loop {
                let result = match stream.next().await {
                    Some(Ok(event)) => event,
                    Some(Err(err)) => {
                        self.bus.publish(Event::Error(err.to_string()));
                        return;
                    }
                    None => {
                        self.bus.publish(Event::Error(
                            ModelError::StreamProtocol("stream ended without Completed event")
                                .to_string(),
                        ));
                        return;
                    }
                };

                match result {
                    ResponseEvent::TextDelta(delta) => {
                        self.bus.publish(Event::TextDelta(delta));
                    }
                    ResponseEvent::ToolCallStart { id, name } => {
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
                        let call = ToolCall::new(id, name, arguments);
                        self.session.push(Item::tool_call(call.clone()));
                        tool_calls.push(call);
                    }
                    ResponseEvent::Cancelled => {
                        self.bus.publish(Event::TurnInterrupted);
                        return;
                    }
                    ResponseEvent::TextDone(text) => {
                        self.session.push(Item::assistant(text.clone()));
                        self.bus.publish(Event::TextDone(text));
                    }
                    ResponseEvent::Completed {
                        usage,
                        finish_reason: _,
                    } => {
                        break usage;
                    }
                }
            };

            // 正常完成：记录 token 使用
            if let Some(u) = usage {
                self.session.record_usage(u);
            }

            if tool_calls.is_empty() {
                // 无工具调用 → 正常完成
                let final_usage = usage;
                self.bus.publish(Event::TurnComplete { usage: final_usage });
                return;
            }

            // 执行工具
            let results = self.execute_tools(&tool_calls).await;

            // 将工具结果推入 session
            for result in results {
                self.session.push(Item::tool_result(result));
            }

            // 继续下一轮（模型将看到工具结果并决定下一步）
            let _ = turn; // turn 仅用于 max_turns 计数
        }

        // 超过 max_turns
        self.bus.publish(Event::Error(format!(
            "max turns reached ({})",
            self.max_turns
        )));
    }

    // ==================== 工具执行 ====================

    /// 执行工具调用列表，返回 `ToolResult` 列表。
    ///
    /// Phase 1 串行执行。Phase 2 加入 `is_concurrency_safe` 分区并行。
    ///
    /// 参考:
    /// - Claude Code `toolOrchestration.ts`: `runToolsSerially()`
    /// - Codex CLI: `FuturesOrdered` 并行执行
    async fn execute_tools(&self, calls: &[ToolCall]) -> Vec<ToolResult> {
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

            results.push(result);
        }

        results
    }
}

// ==================== Tests ====================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ModelError, ModelProvider, ModelRequest, ModelResponse};
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
                items: vec![Item::assistant(&self.response)],
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
            _cancel: CancellationToken,
        ) -> Result<ResponseStream, ModelError> {
            let (tx, rx) = tokio::sync::mpsc::channel(32);

            let text = self.response.clone();
            tokio::spawn(async move {
                let _ = tx.send(Ok(ResponseEvent::TextDelta(text.clone()))).await;
                let _ = tx
                    .send(Ok(ResponseEvent::TextDone(text)))
                    .await;
                let _ = tx
                    .send(Ok(ResponseEvent::Completed {
                        finish_reason: Some("stop".to_string()),
                        usage: Some(TokenUsage {
                            input_tokens: Some(10),
                            output_tokens: Some(5),
                            total_tokens: Some(15),
                        }),
                    }))
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
                items: vec![Item::assistant("done")],
                finish_reason: Some("stop".to_string()),
                usage: None,
            })
        }

        async fn stream(
            &self,
            _model: &str,
            _request: ModelRequest,
            _cancel: CancellationToken,
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
                    .send(Ok(ResponseEvent::Completed {
                        finish_reason: Some("tool_calls".to_string()),
                        usage: Some(TokenUsage {
                            input_tokens: Some(50),
                            output_tokens: Some(20),
                            total_tokens: Some(70),
                        }),
                    }))
                    .await;
            });
            Ok(ResponseStream::new(rx))
        }
    }

    /// Mock provider: 发送一个 TextDelta 后自行取消 token（模拟外部中断）。
    struct SlowProvider;

    #[async_trait]
    impl ModelProvider for SlowProvider {
        async fn send(
            &self,
            _model: &str,
            _request: ModelRequest,
        ) -> Result<ModelResponse, ModelError> {
            Ok(ModelResponse {
                items: vec![Item::assistant("slow")],
                finish_reason: Some("stop".to_string()),
                usage: None,
            })
        }

        async fn stream(
            &self,
            _model: &str,
            _request: ModelRequest,
            cancel: CancellationToken,
        ) -> Result<ResponseStream, ModelError> {
            let (tx, rx) = tokio::sync::mpsc::channel(32);
            // Provider 持有 cancel 的 clone，发送 TextDelta 后自行取消
            // 模拟"用户在流式输出过程中按下中断"的场景
            let cancel_trigger = cancel.clone();
            tokio::spawn(async move {
                let _ = tx
                    .send(Ok(ResponseEvent::TextDelta("partial".to_string())))
                    .await;
                cancel_trigger.cancel();
                let _ = tx.send(Ok(ResponseEvent::Cancelled)).await;
            });
            Ok(ResponseStream::new(rx))
        }
    }

    /// Mock provider: 流意外结束，不发送任何终态事件。
    struct MissingTerminalProvider;

    #[async_trait]
    impl ModelProvider for MissingTerminalProvider {
        async fn send(
            &self,
            _model: &str,
            _request: ModelRequest,
        ) -> Result<ModelResponse, ModelError> {
            unreachable!("stream-only test provider")
        }

        async fn stream(
            &self,
            _model: &str,
            _request: ModelRequest,
            _cancel: CancellationToken,
        ) -> Result<ResponseStream, ModelError> {
            let (tx, rx) = tokio::sync::mpsc::channel(32);
            tokio::spawn(async move {
                let _ = tx
                    .send(Ok(ResponseEvent::TextDelta("partial".to_string())))
                    .await;
            });
            Ok(ResponseStream::new(rx))
        }
    }

    /// Mock provider: 先产出完成态事件，再发生晚到取消。
    struct LateCancelAfterDoneProvider;

    #[async_trait]
    impl ModelProvider for LateCancelAfterDoneProvider {
        async fn send(
            &self,
            _model: &str,
            _request: ModelRequest,
        ) -> Result<ModelResponse, ModelError> {
            unreachable!("stream-only test provider")
        }

        async fn stream(
            &self,
            _model: &str,
            _request: ModelRequest,
            cancel: CancellationToken,
        ) -> Result<ResponseStream, ModelError> {
            let (tx, rx) = tokio::sync::mpsc::channel(32);
            tokio::spawn(async move {
                let _ = tx
                    .send(Ok(ResponseEvent::TextDelta("done".to_string())))
                    .await;
                let _ = tx
                    .send(Ok(ResponseEvent::TextDone("done".to_string())))
                    .await;
                let _ = tx
                    .send(Ok(ResponseEvent::Completed {
                        finish_reason: Some("stop".to_string()),
                        usage: Some(TokenUsage {
                            input_tokens: Some(1),
                            output_tokens: Some(1),
                            total_tokens: Some(2),
                        }),
                    }))
                    .await;
                cancel.cancel();
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
        assert!(matches!(&events[3], Event::TurnComplete { usage: Some(_) }));

        // session 应该有 2 个 item: user + assistant
        assert_eq!(agent.session().len(), 2);
        assert_eq!(agent.session().total_tokens(), 15);
    }

    #[tokio::test]
    async fn cancel_token_works() {
        let mut agent = text_agent("response");

        // cancel 默认未取消
        assert!(!agent.cancel.is_cancelled());

        // 发送中断
        agent.submit(Op::Interrupt).await;
        assert!(agent.cancel.is_cancelled());

        // 正常 submit 会重置 cancel token 并正常完成
        let mut sub = agent.bus().subscribe();
        agent.submit(Op::UserTurn("hi".to_string())).await;

        // 重置后应该正常完成
        assert!(!agent.cancel.is_cancelled());
        let events = collect_events(&mut sub, 4).await;
        assert!(events.contains(&Event::TurnStarted));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::TurnComplete { .. }))
        );
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
        let items = agent.session().items();
        assert!(items.len() >= 3);
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

        let msgs = agent.session().items();
        // user + tool_call + tool_result
        assert_eq!(msgs.len(), 3);

        match &msgs[2] {
            Item::ToolResult(result) => {
                assert_eq!(result.tool_name, "echo");
                assert!(!result.is_error);
                assert!(result.content.contains("hello"));
            }
            _ => panic!("expected ToolResult item"),
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

    #[tokio::test]
    async fn mid_stream_interrupt_skips_session_push() {
        let model = Model::new(Box::new(SlowProvider), "test-model").unwrap();
        let session = Session::new("system", 100_000);
        let registry = ToolRegistry::new();
        let bus = Bus::new(64);
        let mut agent = Agent::new(model, session, registry, bus, 10);
        let mut sub = agent.bus().subscribe();

        // SlowProvider 发送一个 TextDelta 后自行取消 token
        // 模拟"流式输出过程中被中断"的场景
        agent
            .submit(Op::UserTurn("test interrupt".to_string()))
            .await;

        let events = collect_events(&mut sub, 5).await;

        // 应该收到 TurnStarted 和 TurnInterrupted
        assert!(events.contains(&Event::TurnStarted));
        assert!(events.iter().any(|e| matches!(e, Event::TurnInterrupted)));

        // 不应该收到 TurnComplete
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::TurnComplete { .. }))
        );

        // session 不应该包含半截 assistant item
        // 只有 user item
        assert_eq!(agent.session().len(), 1);
        assert!(matches!(
            agent.session().items()[0],
            Item::Message(Message::User(_))
        ));
    }

    #[tokio::test]
    async fn eof_without_terminal_event_reports_protocol_error() {
        let model = Model::new(Box::new(MissingTerminalProvider), "test-model").unwrap();
        let session = Session::new("system", 100_000);
        let registry = ToolRegistry::new();
        let bus = Bus::new(64);
        let mut agent = Agent::new(model, session, registry, bus, 10);
        let mut sub = agent.bus().subscribe();

        agent.submit(Op::UserTurn("test eof".to_string())).await;

        let events = collect_events(&mut sub, 5).await;
        assert!(events.contains(&Event::TurnStarted));
        assert!(events
            .iter()
            .any(|e| matches!(e, Event::Error(msg) if msg == "stream protocol error: stream ended without Completed event")));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::TurnComplete { .. }))
        );
        assert_eq!(agent.session().len(), 1);
    }

    #[tokio::test]
    async fn buffered_message_done_wins_over_late_cancel() {
        let model = Model::new(Box::new(LateCancelAfterDoneProvider), "test-model").unwrap();
        let session = Session::new("system", 100_000);
        let registry = ToolRegistry::new();
        let bus = Bus::new(64);
        let mut agent = Agent::new(model, session, registry, bus, 10);
        let mut sub = agent.bus().subscribe();

        agent
            .submit(Op::UserTurn("test late cancel".to_string()))
            .await;

        let events = collect_events(&mut sub, 5).await;
        assert!(events.contains(&Event::TurnStarted));
        assert!(events.contains(&Event::TextDelta("done".to_string())));
        assert!(events.contains(&Event::TextDone("done".to_string())));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::TurnComplete { .. }))
        );
        assert!(!events.iter().any(|e| matches!(e, Event::TurnInterrupted)));
        assert_eq!(agent.session().len(), 2);
    }
}
