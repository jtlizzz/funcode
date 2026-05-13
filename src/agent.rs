//! Agent orchestration core.
//!
//! Implements the Agent loop: receive user operations -> stream model output -> execute tools ->
//! continue or stop. Publishes real-time events to external consumers through [`Bus`].
//!
//! # Core Loop
//!
//! ```text
//! loop {
//!     request = session.build_request(registry.specs())
//!     stream = model.stream(request)
//!     (tool_calls, usage) = inline stream event loop
//!     if has ToolCall {
//!         results = execute_tools(tool_calls)
//!         session.push(ToolResult)
//!         continue
//!     } else {
//!         break
//!     }
//! }
//! ```
//!
//! References:
//! - Claude Code `src/query.ts` — `queryLoop()` AsyncGenerator
//! - Codex CLI `codex-rs/core/src/codex_thread.rs` — `run_turn()`
//! - OpenCode `session/prompt.ts` — `runLoop()`

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::event::Event;
use crate::model::{
    Item, Model, ModelError, ResponseEvent, TokenUsage, ToolCall,
    ToolResult,
};
use crate::session::Session;
use crate::tools::{ToolContext, ToolRegistry};

// ==================== Op Enum ====================

/// Agent operation submitted externally through [`Agent::submit`].
///
/// Follows Codex CLI's unified `Op` enum pattern: all operations enter through one boundary.
/// Claude Code uses separate mechanisms (`AbortController` + `setModel()`); funcode keeps the
/// Codex-style unified operation model while the direct-submit entry point stays minimal.
pub enum Op {
    /// User text that starts a new conversation turn.
    UserTurn(String),
    /// Shut down the Agent background loop.
    Shutdown,
}

impl Op {
    /// Create a user turn operation without a return channel.
    pub fn user_turn(text: impl Into<String>) -> Self {
        Self::UserTurn(text.into())
    }
}

/// Structured result for one Agent turn.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnOutcome {
    Completed { usage: Option<TokenUsage> },
    Interrupted,
    Failed(String),
    MaxTurnsReached { max_turns: usize },
}

type SharedCancelToken = Arc<Mutex<CancellationToken>>;

// ==================== Agent Handle ====================

/// Agent handle held by external callers.
///
/// Wraps both communication channels: `op_tx` for sending operations to the Agent,
/// and `event_rx` for receiving observation events. Clone-able via `Arc<Mutex<Receiver>>`.
///
/// Reference: DeepSeek-TUI `EngineHandle` — same pattern.
#[derive(Clone)]
pub struct AgentHandle {
    op_tx: mpsc::Sender<Op>,
    event_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<Event>>>,
    cancel: SharedCancelToken,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentHandleError {
    #[error("agent is closed")]
    Closed,
}

impl AgentHandle {
    /// Receive the next agent event.
    pub async fn recv_event(&self) -> Option<Event> {
        self.event_rx.lock().await.recv().await
    }

    /// Submit one user input turn.
    pub async fn user_turn(&self, text: String) -> Result<(), AgentHandleError> {
        self.op_tx
            .send(Op::UserTurn(text))
            .await
            .map_err(|_| AgentHandleError::Closed)
    }

    /// Immediately cancel the current turn.
    pub async fn interrupt(&self) -> Result<(), AgentHandleError> {
        if self.op_tx.is_closed() {
            return Err(AgentHandleError::Closed);
        }

        let cancel = self
            .cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cancel.cancel();
        Ok(())
    }

    /// Request shutdown of the Agent background loop.
    pub async fn shutdown(&self) -> Result<(), AgentHandleError> {
        self.op_tx
            .send(Op::Shutdown)
            .await
            .map_err(|_| AgentHandleError::Closed)
    }
}

// ==================== Agent ====================

/// Core Agent object that connects Model / Session / ToolRegistry.
pub struct Agent {
    model: Model,
    session: Session,
    registry: ToolRegistry,
    event_tx: mpsc::Sender<Event>,
    max_turns: usize,
    /// Current turn cancellation handle. `AgentHandle::interrupt()` uses it as an independent
    /// cancellation path.
    cancel: SharedCancelToken,
}

impl Agent {
    /// Emit an event.
    fn emit(&self, event: Event) {
        let _ = self.event_tx.try_send(event);
    }

    /// Return a read-only session reference.
    fn session(&self) -> &Session {
        &self.session
    }

    /// Submit one operation.
    ///
    /// This is the single direct-call entry point for Agent operations:
    /// - `Op::user_turn(text)` -> start one conversation turn
    async fn submit(&mut self, op: Op) -> TurnOutcome {
        match op {
            Op::UserTurn(text) => {
                self.session.push(Item::user(text));
                self.run_turn().await
            }
            Op::Shutdown => TurnOutcome::Completed { usage: None },
        }
    }

    /// Create an Agent and start its background loop, returning a cloneable control handle.
    ///
    /// This is the only public constructor. Creates both communication channels internally:
    /// op channel for sending operations, event channel for receiving observation events.
    ///
    /// Reference: DeepSeek-TUI `spawn_engine()` — same pattern.
    pub fn spawn(
        model: Model,
        session: Session,
        registry: ToolRegistry,
        max_turns: usize,
        queue_capacity: usize,
    ) -> AgentHandle {
        let (op_tx, op_rx) = mpsc::channel(queue_capacity);
        let (event_tx, event_rx) = mpsc::channel(256);
        let cancel = Arc::new(Mutex::new(CancellationToken::new()));

        let agent = Self {
            model,
            session,
            registry,
            event_tx,
            max_turns,
            cancel: cancel.clone(),
        };

        tokio::spawn(async move {
            agent.run_op_loop(op_rx).await;
        });

        AgentHandle {
            op_tx,
            event_rx: Arc::new(tokio::sync::Mutex::new(event_rx)),
            cancel,
        }
    }

    async fn run_op_loop(mut self, mut rx: mpsc::Receiver<Op>) {
        while let Some(op) = rx.recv().await {
            match op {
                Op::UserTurn(text) => {
                    self.session.push(Item::user(text));
                    let _ = self.run_turn().await;
                }
                Op::Shutdown => {
                    self.cancel_current_turn();
                    break;
                }
            }
        }
    }

    fn reset_cancel(&mut self) {
        let mut shared = self
            .cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *shared = CancellationToken::new();
    }

    fn cancel(&self) -> CancellationToken {
        self.cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn is_cancelled(&self) -> bool {
        self.cancel().is_cancelled()
    }

    fn cancel_current_turn(&self) {
        let cancel = self
            .cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cancel.cancel();
    }

    // ==================== Core Loop ====================

    /// Agent main loop.
    ///
    /// Repeats: build request -> stream model output -> consume events -> execute tools ->
    /// continue or stop.
    ///
    /// Reference: Claude Code `query.ts`'s `while (true)` loop:
    /// ```ignore
    /// while (true) {
    ///     for await (const item of deps.callModel({ messages, tools })) {
    ///         if (tool_calls found) needsFollowUp = true
    ///     }
    ///     if (needsFollowUp) { execute tools; state = next; continue }
    ///     else { return { reason: 'completed' } }
    /// }
    /// ```
    async fn run_turn(&mut self) -> TurnOutcome {
        // Reset cancellation: each new turn gets a fresh token.
        self.reset_cancel();

        self.emit(Event::TurnStarted);

        for turn in 0..self.max_turns {
            // Check for interruption.
            if self.is_cancelled() {
                self.emit(Event::TurnInterrupted);
                return TurnOutcome::Interrupted;
            }

            // Enforce the session token budget.
            self.session.truncate_to_budget();

            // Build the model request.
            let tools = self.registry.specs();
            let request = self.session.build_request(&tools);

            // Stream model output with the current cancellation token.
            let cancel = self.cancel();
            let mut stream = match self.model.stream(request, cancel).await {
                Ok(s) => s,
                Err(err) => {
                    self.emit(Event::Error(err.to_string()));
                    return TurnOutcome::Failed(err.to_string());
                }
            };

            // Consume stream events. TextDone / ToolCallReady / Completed are authoritative
            // terminal item states.
            let mut tool_calls = Vec::new();
            let usage = loop {
                let result = match stream.next().await {
                    Some(Ok(event)) => event,
                    Some(Err(err)) => {
                        self.emit(Event::Error(err.to_string()));
                        return TurnOutcome::Failed(err.to_string());
                    }
                    None => {
                        let error =
                            ModelError::StreamProtocol("stream ended without Completed event")
                                .to_string();
                        self.emit(Event::Error(error.clone()));
                        return TurnOutcome::Failed(error);
                    }
                };

                match result {
                    ResponseEvent::TextDelta(delta) => {
                        self.emit(Event::TextDelta(delta));
                    }
                    ResponseEvent::ToolCallStart { id, name } => {
                        self.emit(Event::ToolCallBegin {
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
                        self.emit(Event::TurnInterrupted);
                        return TurnOutcome::Interrupted;
                    }
                    ResponseEvent::TextDone(text) => {
                        self.session.push(Item::assistant(text.clone()));
                        self.emit(Event::TextDone(text));
                    }
                    ResponseEvent::Completed {
                        usage,
                        finish_reason: _,
                    } => {
                        break usage;
                    }
                }
            };

            // Record token usage for a completed model response.
            if let Some(u) = usage {
                self.session.record_usage(u);
            }

            if tool_calls.is_empty() {
                // No tool calls means the turn completed normally.
                let final_usage = usage;
                self.emit(Event::TurnComplete { usage: final_usage });
                return TurnOutcome::Completed { usage: final_usage };
            }

            // Execute tools.
            let results = self.execute_tools(&tool_calls).await;

            // Push tool results into the session.
            for result in results {
                self.session.push(Item::tool_result(result));
            }

            // Continue to the next loop; the model will see the tool results.
            let _ = turn; // `turn` is only used for max_turns counting.
        }

        // Exceeded max_turns.
        let error = format!("max turns reached ({})", self.max_turns);
        self.emit(Event::Error(error));
        TurnOutcome::MaxTurnsReached {
            max_turns: self.max_turns,
        }
    }

    // ==================== Tool Execution ====================

    /// Execute tool calls and return their `ToolResult`s.
    ///
    /// Phase 1 executes tools serially. Phase 2 will add partitioned parallel execution via
    /// `is_concurrency_safe`.
    ///
    /// References:
    /// - Claude Code `toolOrchestration.ts`: `runToolsSerially()`
    /// - Codex CLI: parallel execution with `FuturesOrdered`
    async fn execute_tools(&self, calls: &[ToolCall]) -> Vec<ToolResult> {
        let mut results = Vec::with_capacity(calls.len());

        for call in calls {
            let result = self
                .registry
                .execute_with_context(
                    &call.id,
                    &call.name,
                    &call.arguments,
                    ToolContext::new(self.cancel()),
                )
                .await;

            self.emit(Event::ToolCallEnd {
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
    use crate::model::{Message, ModelError, ModelProvider, ModelRequest, ModelResponse, ResponseStream};
    use crate::tools::Tool;
    use async_trait::async_trait;
    use serde_json::json;

    // === Mock Provider ===

    /// Mock provider: returns a plain text response without tool calls.
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
                let _ = tx.send(Ok(ResponseEvent::TextDone(text))).await;
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

    /// Mock provider: always returns one tool call.
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

    /// Mock provider: sends one TextDelta, then cancels the token itself to simulate an external
    /// interrupt.
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
            // The provider owns a clone of the token, sends TextDelta, then cancels itself.
            // This simulates a user interrupt during streaming output.
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

    /// Mock provider: stream ends unexpectedly without sending any terminal event.
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

    /// Mock provider: emits terminal events first, then triggers a late cancellation.
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

    /// Mock provider: waits for external cancellation before ending.
    struct BlockingUntilCancelProvider;

    #[async_trait]
    impl ModelProvider for BlockingUntilCancelProvider {
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
                    .send(Ok(ResponseEvent::TextDelta("started".to_string())))
                    .await;
                cancel.cancelled().await;
                let _ = tx.send(Ok(ResponseEvent::Cancelled)).await;
            });
            Ok(ResponseStream::new(rx))
        }
    }

    // === Helpers ===

    fn text_agent(response: &str) -> (Agent, mpsc::Receiver<Event>) {
        let model = Model::new(
            Box::new(TextProvider {
                response: response.to_string(),
            }),
            "test-model",
        )
        .unwrap();
        let session = Session::new("You are helpful.", 100_000);
        let registry = ToolRegistry::new();
        let (event_tx, event_rx) = mpsc::channel(64);
        let agent = Agent {
            model,
            session,
            registry,
            event_tx,
            max_turns: 10,
            cancel: Arc::new(Mutex::new(CancellationToken::new())),
        };
        (agent, event_rx)
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
        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {"message": {"type": "string"}}})
        }
        async fn execute(
            &self,
            args: &str,
            _context: crate::tools::ToolContext,
        ) -> Result<String, crate::tools::ToolError> {
            Ok(args.to_string())
        }
    }

    async fn collect_events(event_rx: &mut mpsc::Receiver<Event>, max: usize) -> Vec<Event> {
        let mut events = Vec::with_capacity(max);
        for _ in 0..max {
            match tokio::time::timeout(std::time::Duration::from_millis(500), event_rx.recv()).await {
                Ok(Some(e)) => events.push(e),
                _ => break,
            }
        }
        events
    }

    async fn collect_handle_events(handle: &AgentHandle, max: usize) -> Vec<Event> {
        let mut events = Vec::with_capacity(max);
        for _ in 0..max {
            match tokio::time::timeout(std::time::Duration::from_millis(500), handle.recv_event()).await {
                Ok(Some(e)) => events.push(e),
                _ => break,
            }
        }
        events
    }

    // === Tests ===

    #[tokio::test]
    async fn text_only_turn_completes() {
        let (mut agent, mut event_rx) = text_agent("Hello world");

        agent.submit(Op::user_turn("hi")).await;

        // Expected events: TurnStarted -> TextDelta -> TextDone -> TurnComplete.
        let events = collect_events(&mut event_rx, 4).await;
        assert!(events.contains(&Event::TurnStarted));
        assert!(events.contains(&Event::TextDelta("Hello world".to_string())));
        assert!(events.contains(&Event::TextDone("Hello world".to_string())));
        assert!(matches!(&events[3], Event::TurnComplete { usage: Some(_) }));

        // Session should contain 2 items: user + assistant.
        assert_eq!(agent.session().len(), 2);
        assert_eq!(agent.session().total_tokens(), 15);
    }

    #[tokio::test]
    async fn agent_handle_user_turn_completes() {
        let handle = Agent::spawn(
            Model::new(Box::new(TextProvider { response: "Hello from handle".to_string() }), "test-model").unwrap(),
            Session::new("You are helpful.", 100_000),
            ToolRegistry::new(),
            10,
            16,
        );

        handle
            .user_turn("hi".to_string())
            .await
            .expect("handle should enqueue turn");

        let events = collect_handle_events(&handle, 4).await;
        assert!(events.contains(&Event::TextDone("Hello from handle".to_string())));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::TurnComplete { usage: Some(_) }))
        );
    }

    #[tokio::test]
    async fn agent_handle_rejects_after_loop_closes() {
        let handle = Agent::spawn(
            Model::new(Box::new(TextProvider { response: "bye".to_string() }), "test-model").unwrap(),
            Session::new("system", 100_000),
            ToolRegistry::new(),
            10,
            1,
        );

        handle.shutdown().await.expect("shutdown op should send");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let err = handle
            .user_turn("after shutdown".to_string())
            .await
            .expect_err("closed handle should return error");

        assert_eq!(err, AgentHandleError::Closed);
    }

    #[tokio::test]
    async fn handle_interrupt_cancels_running_turn() {
        let handle = Agent::spawn(
            Model::new(Box::new(BlockingUntilCancelProvider), "test-model").unwrap(),
            Session::new("system", 100_000),
            ToolRegistry::new(),
            10,
            16,
        );

        let turn_handle = {
            let handle = handle.clone();
            tokio::spawn(async move { handle.user_turn("block".to_string()).await })
        };

        let first_events = collect_handle_events(&handle, 2).await;
        assert!(first_events.contains(&Event::TurnStarted));
        assert!(first_events.contains(&Event::TextDelta("started".to_string())));

        handle.interrupt().await.expect("interrupt should send");

        tokio::time::timeout(std::time::Duration::from_secs(1), turn_handle)
            .await
            .expect("turn should finish after interrupt")
            .expect("task should join")
            .expect("turn should enqueue");

        let events = collect_handle_events(&handle, 2).await;
        assert!(events.iter().any(|e| matches!(e, Event::TurnInterrupted)));
    }

    #[tokio::test]
    async fn cancel_token_works() {
        let (mut agent, mut event_rx) = text_agent("response");

        // Cancellation starts unset.
        assert!(!agent.is_cancelled());

        // Cancel directly.
        agent.cancel_current_turn();
        assert!(agent.is_cancelled());

        // A normal submit resets the cancellation token and completes.
        agent.submit(Op::user_turn("hi")).await;

        // After reset, the turn should complete normally.
        assert!(!agent.is_cancelled());
        let events = collect_events(&mut event_rx, 4).await;
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
        let (event_tx, _event_rx) = mpsc::channel(64);
        let mut agent = Agent {
            model,
            session,
            registry,
            event_tx,
            max_turns: 2,
            cancel: Arc::new(Mutex::new(CancellationToken::new())),
        };

        agent.submit(Op::user_turn("use tool")).await;

        // max_turns=2 and ToolCallProvider returns a tool call every time.
        // It should stop after 2 loops because max_turns is reached.
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
        let (event_tx, _event_rx) = mpsc::channel(64);
        let mut agent = Agent {
            model,
            session,
            registry,
            event_tx,
            max_turns: 1,
            cancel: Arc::new(Mutex::new(CancellationToken::new())),
        };

        agent.submit(Op::user_turn("use echo")).await;

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
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let mut agent = Agent {
            model,
            session,
            registry,
            event_tx,
            max_turns: 1,
            cancel: Arc::new(Mutex::new(CancellationToken::new())),
        };

        agent.submit(Op::user_turn("go")).await;

        let events = collect_events(&mut event_rx, 5).await;
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
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let mut agent = Agent {
            model,
            session,
            registry,
            event_tx,
            max_turns: 10,
            cancel: Arc::new(Mutex::new(CancellationToken::new())),
        };

        // SlowProvider sends one TextDelta and then cancels the token itself.
        // This simulates an interrupt during streaming output.
        agent.submit(Op::user_turn("test interrupt")).await;

        let events = collect_events(&mut event_rx, 5).await;

        // Should receive TurnStarted and TurnInterrupted.
        assert!(events.contains(&Event::TurnStarted));
        assert!(events.iter().any(|e| matches!(e, Event::TurnInterrupted)));

        // Should not receive TurnComplete.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::TurnComplete { .. }))
        );

        // Session should not contain a partial assistant item, only the user item.
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
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let mut agent = Agent {
            model,
            session,
            registry,
            event_tx,
            max_turns: 10,
            cancel: Arc::new(Mutex::new(CancellationToken::new())),
        };

        agent.submit(Op::user_turn("test eof")).await;

        let events = collect_events(&mut event_rx, 5).await;
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
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let mut agent = Agent {
            model,
            session,
            registry,
            event_tx,
            max_turns: 10,
            cancel: Arc::new(Mutex::new(CancellationToken::new())),
        };

        agent.submit(Op::user_turn("test late cancel")).await;

        let events = collect_events(&mut event_rx, 5).await;
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
