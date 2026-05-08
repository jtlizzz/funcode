# Agent Op Queue TUI Interaction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Convert funcode from direct `Agent::submit(&mut self, Op)` calls toward a queue-driven Agent handle so TUI can send `UserTurn` and `Interrupt` without borrowing Agent state directly.

**Architecture:** Introduce an `AgentHandle` backed by `tokio::sync::mpsc` and run an Agent-owned background submission loop. Keep the current model-tool loop logic inside `Agent` for now, but make `UserTurn` run as an active turn task so `Interrupt` can be processed while the turn is running. Preserve `Bus` as the Agent -> TUI observation event channel.

**Tech Stack:** Rust 2024, Tokio `mpsc`/`oneshot`, `tokio_util::sync::CancellationToken`, existing `Bus`, existing `Model`/`Session`/`ToolRegistry` abstractions.

---

## Scope

This plan only covers the queue-driven Agent/TUI interaction skeleton. It does not implement shell, write tools, approval UI, or a polished TUI. The model-tool loop must continue to pass existing tests after every task.

The target communication model is:

```text
TUI / CLI input
  -> AgentHandle::submit(Op)
  -> Agent submission loop
  -> active turn task
  -> Model / Tools / Session
  -> Bus events
  -> TUI / CLI renderer
```

## File Structure

- Modify `src/agent.rs`: add `AgentHandle`, `TurnOutcome`, op queue spawn logic, active turn cancellation, and tests.
- Modify `src/bus.rs`: keep existing event model; optionally add no new events in this plan.
- Modify `src/tui.rs`: add a minimal TUI/CLI-facing runner API that sends `Op` through `AgentHandle` instead of borrowing `Agent` directly.
- Modify `src/app.rs`: later task wires `Agent::spawn()` / `AgentHandle` into app assembly.
- Modify `src/main.rs`: no functional change until app/config exist; keep module wiring compiling.
- Modify `Cargo.toml`: only if Tokio features are missing. Current `tokio` features already include `macros` and `rt-multi-thread`; add `sync` if compilation requires explicit feature gating.
- Test primarily through `cargo test agent` and then full `cargo test`.

## Design Notes

- Keep `Agent` as the domain object that owns `Model`, `Session`, `ToolRegistry`, `Bus`, `max_turns`, and cancellation state.
- Add a small handle object for external callers. The handle should not expose `Session` or mutable `Agent` internals.
- Use `mpsc` for inbound ops and `oneshot` for per-turn outcome.
- Do not use `Arc<Mutex<Agent>>`.
- Add reference comments near the new queue-pair design:
  - `参考: /home/acer/project/rust_project/codex-main/codex-rs/core/src/codex.rs` for `Codex { tx_sub, rx_event }`.
  - `参考: /home/acer/project/rust_project/codex-main/codex-rs/core/src/tasks/mod.rs` for active turn cancellation.
  - `参考: /home/acer/project/rust_project/codex-main/codex-rs/tui/src/chatwidget/agent.rs` for TUI op forwarding.

---

### Task 1: Add TurnOutcome Without Changing Queue Semantics

**Files:**
- Modify: `src/agent.rs`
- Test: existing `src/agent.rs` tests

- [ ] **Step 1: Add failing tests for structured outcomes**

Add these tests in `src/agent.rs` under the existing `#[cfg(test)] mod tests` section:

```rust
#[tokio::test]
async fn submit_user_turn_returns_completed_outcome() {
    let mut agent = text_agent("Hello world");

    let outcome = agent.submit(Op::UserTurn("hi".to_string())).await;

    assert!(matches!(outcome, TurnOutcome::Completed { usage: Some(_) }));
}

#[tokio::test]
async fn submit_user_turn_returns_interrupted_outcome() {
    let model = Model::new(Box::new(SlowProvider), "test-model").unwrap();
    let session = Session::new("system", 100_000);
    let registry = ToolRegistry::new();
    let bus = Bus::new(64);
    let mut agent = Agent::new(model, session, registry, bus, 10);

    let outcome = agent
        .submit(Op::UserTurn("test interrupt".to_string()))
        .await;

    assert_eq!(outcome, TurnOutcome::Interrupted);
}

#[tokio::test]
async fn submit_user_turn_returns_max_turns_outcome() {
    let model = Model::new(Box::new(ToolCallProvider), "test-model").unwrap();
    let session = Session::new("system", 100_000);
    let registry = {
        let mut r = ToolRegistry::new();
        r.register(Box::new(EchoTool));
        r
    };
    let bus = Bus::new(64);
    let mut agent = Agent::new(model, session, registry, bus, 1);

    let outcome = agent.submit(Op::UserTurn("use echo".to_string())).await;

    assert_eq!(outcome, TurnOutcome::MaxTurnsReached { max_turns: 1 });
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cargo test agent::tests::submit_user_turn_returns_completed_outcome agent::tests::submit_user_turn_returns_interrupted_outcome agent::tests::submit_user_turn_returns_max_turns_outcome
```

Expected: compilation fails because `TurnOutcome` does not exist and `submit()` returns `()`.

- [ ] **Step 3: Add `TurnOutcome` and update submit/run_turn signatures**

In `src/agent.rs`, add near the `Op` enum:

```rust
// ==================== Turn Outcome ====================

/// Structured result of a user turn.
///
/// `Bus` events remain the observation channel for UI rendering. `TurnOutcome`
/// is the control-flow result for app/TUI code that needs to know how a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed { usage: Option<TokenUsage> },
    Interrupted,
    Failed(String),
    MaxTurnsReached { max_turns: usize },
}
```

Change `submit()` to return `TurnOutcome`:

```rust
pub async fn submit(&mut self, op: Op) -> TurnOutcome {
    match op {
        Op::UserTurn(text) => {
            self.session.push(Item::user(text));
            self.run_turn().await
        }
        Op::Interrupt => {
            // 参考: /home/acer/project/rust_project/codex-main/codex-rs/core/src/tasks/mod.rs
            self.cancel.cancel();
            TurnOutcome::Interrupted
        }
    }
}
```

Change `run_turn()` to return `TurnOutcome`. Replace each early `return;` with the appropriate result:

```rust
async fn run_turn(&mut self) -> TurnOutcome {
    self.cancel = CancellationToken::new();
    self.bus.publish(Event::TurnStarted);

    for _turn in 0..self.max_turns {
        if self.cancel.is_cancelled() {
            self.bus.publish(Event::TurnInterrupted);
            return TurnOutcome::Interrupted;
        }

        self.session.truncate_to_budget();
        let tools = self.registry.specs();
        let request = self.session.build_request(&tools);
        let cancel = self.cancel.clone();
        let mut stream = match self.model.stream(request, cancel).await {
            Ok(s) => s,
            Err(err) => {
                let message = err.to_string();
                self.bus.publish(Event::Error(message.clone()));
                return TurnOutcome::Failed(message);
            }
        };

        let mut tool_calls = Vec::new();
        let usage = loop {
            let result = match stream.next().await {
                Some(Ok(event)) => event,
                Some(Err(err)) => {
                    let message = err.to_string();
                    self.bus.publish(Event::Error(message.clone()));
                    return TurnOutcome::Failed(message);
                }
                None => {
                    let message = ModelError::StreamProtocol(
                        "stream ended without Completed event",
                    )
                    .to_string();
                    self.bus.publish(Event::Error(message.clone()));
                    return TurnOutcome::Failed(message);
                }
            };

            match result {
                ResponseEvent::TextDelta(delta) => self.bus.publish(Event::TextDelta(delta)),
                ResponseEvent::ToolCallStart { id, name } => {
                    self.bus.publish(Event::ToolCallBegin { id, name });
                }
                ResponseEvent::ToolCallReady { id, name, arguments } => {
                    let call = ToolCall::new(id, name, arguments);
                    self.session.push(Item::tool_call(call.clone()));
                    tool_calls.push(call);
                }
                ResponseEvent::Cancelled => {
                    self.bus.publish(Event::TurnInterrupted);
                    return TurnOutcome::Interrupted;
                }
                ResponseEvent::TextDone(text) => {
                    self.session.push(Item::assistant(text.clone()));
                    self.bus.publish(Event::TextDone(text));
                }
                ResponseEvent::Completed { usage, finish_reason: _ } => break usage,
            }
        };

        if let Some(u) = usage {
            self.session.record_usage(u);
        }

        if tool_calls.is_empty() {
            self.bus.publish(Event::TurnComplete { usage });
            return TurnOutcome::Completed { usage };
        }

        let results = self.execute_tools(&tool_calls).await;
        for result in results {
            self.session.push(Item::tool_result(result));
        }
    }

    let message = format!("max turns reached ({})", self.max_turns);
    self.bus.publish(Event::Error(message));
    TurnOutcome::MaxTurnsReached {
        max_turns: self.max_turns,
    }
}
```

- [ ] **Step 4: Update existing tests that ignore submit result**

Existing tests can leave the return value unused, or explicitly assign it when needed:

```rust
let _ = agent.submit(Op::UserTurn("hi".to_string())).await;
```

No behavioral assertion should be weakened.

- [ ] **Step 5: Run agent tests**

Run:

```bash
cargo test agent
```

Expected: all agent tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/agent.rs
git commit -m "feat: return structured agent turn outcomes"
```

---

### Task 2: Add AgentHandle and an Op Queue Spawn API

**Files:**
- Modify: `src/agent.rs`
- Maybe modify: `Cargo.toml`
- Test: `src/agent.rs`

- [ ] **Step 1: Write failing tests for handle-based submission**

Add these tests to `src/agent.rs` tests:

```rust
#[tokio::test]
async fn agent_handle_user_turn_completes() {
    let agent = text_agent("Hello from handle");
    let handle = Agent::spawn(agent, 16);
    let mut sub = handle.subscribe();

    let outcome = handle
        .user_turn("hi".to_string())
        .await
        .expect("handle should return outcome");

    assert!(matches!(outcome, TurnOutcome::Completed { usage: Some(_) }));
    let events = collect_events(&mut sub, 4).await;
    assert!(events.contains(&Event::TextDone("Hello from handle".to_string())));
}

#[tokio::test]
async fn agent_handle_rejects_after_loop_closes() {
    let agent = text_agent("bye");
    let handle = Agent::spawn(agent, 1);

    handle.shutdown().await.expect("shutdown op should send");
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let err = handle
        .user_turn("after shutdown".to_string())
        .await
        .expect_err("closed handle should return error");

    assert_eq!(err, AgentHandleError::Closed);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run:

```bash
cargo test agent::tests::agent_handle_user_turn_completes agent::tests::agent_handle_rejects_after_loop_closes
```

Expected: compilation fails because `AgentHandle`, `Agent::spawn`, `subscribe`, and `shutdown` do not exist.

- [ ] **Step 3: Extend `Op` for queued turns and shutdown**

Replace the current `Op` enum variants with handle-ready variants while preserving a convenient constructor path:

```rust
pub enum Op {
    UserTurn {
        text: String,
        reply: Option<tokio::sync::oneshot::Sender<TurnOutcome>>,
    },
    Interrupt,
    Shutdown,
}

impl Op {
    pub fn user_turn(text: impl Into<String>) -> Self {
        Self::UserTurn {
            text: text.into(),
            reply: None,
        }
    }
}
```

Then update all existing `Op::UserTurn("...".to_string())` call sites in tests to:

```rust
Op::user_turn("...")
```

- [ ] **Step 4: Add `AgentHandle` and error type**

Add to `src/agent.rs` near the `Agent` struct:

```rust
// ==================== Agent Handle ====================

/// Cloneable external handle for sending operations to the Agent actor.
///
/// 参考: /home/acer/project/rust_project/codex-main/codex-rs/core/src/codex.rs
/// Codex exposes a queue pair: callers submit `Op`, then consume events separately.
#[derive(Clone)]
pub struct AgentHandle {
    tx: tokio::sync::mpsc::Sender<Op>,
    bus: Bus,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AgentHandleError {
    #[error("agent is closed")]
    Closed,
}
```

`Bus` currently is not `Clone`. Add `Clone` to `Bus` in `src/bus.rs`:

```rust
#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<Event>,
}
```

- [ ] **Step 5: Add handle methods**

Add:

```rust
impl AgentHandle {
    pub fn subscribe(&self) -> crate::bus::Subscriber {
        self.bus.subscribe()
    }

    pub async fn user_turn(&self, text: String) -> Result<TurnOutcome, AgentHandleError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Op::UserTurn {
                text,
                reply: Some(reply_tx),
            })
            .await
            .map_err(|_| AgentHandleError::Closed)?;

        reply_rx.await.map_err(|_| AgentHandleError::Closed)
    }

    pub async fn interrupt(&self) -> Result<(), AgentHandleError> {
        self.tx
            .send(Op::Interrupt)
            .await
            .map_err(|_| AgentHandleError::Closed)
    }

    pub async fn shutdown(&self) -> Result<(), AgentHandleError> {
        self.tx
            .send(Op::Shutdown)
            .await
            .map_err(|_| AgentHandleError::Closed)
    }
}
```

- [ ] **Step 6: Add `Agent::spawn()` and queued loop**

Add to `impl Agent`:

```rust
pub fn spawn(self, queue_capacity: usize) -> AgentHandle {
    let bus = self.bus.clone();
    let (tx, rx) = tokio::sync::mpsc::channel(queue_capacity);
    tokio::spawn(async move {
        self.run_op_loop(rx).await;
    });
    AgentHandle { tx, bus }
}

async fn run_op_loop(mut self, mut rx: tokio::sync::mpsc::Receiver<Op>) {
    while let Some(op) = rx.recv().await {
        match op {
            Op::UserTurn { text, reply } => {
                self.session.push(Item::user(text));
                let outcome = self.run_turn().await;
                if let Some(reply) = reply {
                    let _ = reply.send(outcome);
                }
            }
            Op::Interrupt => {
                self.cancel.cancel();
            }
            Op::Shutdown => break,
        }
    }
}
```

Update direct `submit()` to match the new `Op` shape:

```rust
pub async fn submit(&mut self, op: Op) -> TurnOutcome {
    match op {
        Op::UserTurn { text, reply } => {
            self.session.push(Item::user(text));
            let outcome = self.run_turn().await;
            if let Some(reply) = reply {
                let _ = reply.send(outcome.clone());
            }
            outcome
        }
        Op::Interrupt => {
            self.cancel.cancel();
            TurnOutcome::Interrupted
        }
        Op::Shutdown => TurnOutcome::Completed { usage: None },
    }
}
```

- [ ] **Step 7: Run tests**

Run:

```bash
cargo test agent bus
```

Expected: all `agent` and `bus` tests pass.

If compilation fails because `tokio::sync::mpsc` or `oneshot` is gated, modify `Cargo.toml`:

```toml
tokio = { version = "1", features = ["macros", "rt-multi-thread", "sync"] }
```

Then rerun the same command.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml src/agent.rs src/bus.rs
git commit -m "feat: add agent handle op queue"
```

---

### Task 3: Make Interrupt Work While a Turn Is Running

**Files:**
- Modify: `src/agent.rs`
- Test: `src/agent.rs`

- [ ] **Step 1: Add a provider that waits until cancelled**

Add this test provider inside `src/agent.rs` tests:

```rust
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
```

- [ ] **Step 2: Add failing interrupt test**

Add:

```rust
#[tokio::test]
async fn handle_interrupt_cancels_running_turn() {
    let model = Model::new(Box::new(BlockingUntilCancelProvider), "test-model").unwrap();
    let session = Session::new("system", 100_000);
    let registry = ToolRegistry::new();
    let bus = Bus::new(64);
    let agent = Agent::new(model, session, registry, bus, 10);
    let handle = Agent::spawn(agent, 16);
    let mut sub = handle.subscribe();

    let turn_handle = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.user_turn("block".to_string()).await })
    };

    let first_events = collect_events(&mut sub, 2).await;
    assert!(first_events.contains(&Event::TurnStarted));
    assert!(first_events.contains(&Event::TextDelta("started".to_string())));

    handle.interrupt().await.expect("interrupt should send");

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), turn_handle)
        .await
        .expect("turn should finish after interrupt")
        .expect("task should join")
        .expect("turn should return outcome");

    assert_eq!(outcome, TurnOutcome::Interrupted);
}
```

- [ ] **Step 3: Run test to verify it fails or hangs without the fix**

Run:

```bash
cargo test agent::tests::handle_interrupt_cancels_running_turn
```

Expected before implementation: test times out or fails because `run_op_loop()` awaits `run_turn()` and cannot receive `Op::Interrupt` while the turn is running.

- [ ] **Step 4: Track active turn cancellation separately from the op loop**

Add a small active-turn struct in `src/agent.rs`:

```rust
struct ActiveTurn {
    cancel: CancellationToken,
}
```

Add a field to `Agent`:

```rust
active_turn: Option<ActiveTurn>,
```

Initialize it in `Agent::new()`:

```rust
active_turn: None,
```

- [ ] **Step 5: Spawn user turns from the op loop**

Because `Agent` owns `Session`, the clean minimal implementation is to move the running turn into a spawned task and communicate completion back to the op loop. Use a private internal command:

```rust
enum AgentLoopMsg {
    External(Op),
    TurnFinished(TurnOutcome),
}
```

Replace the simple `run_op_loop()` with a loop that receives external ops and turn completion on one internal channel:

```rust
async fn run_op_loop(mut self, mut rx: tokio::sync::mpsc::Receiver<Op>) {
    let (tx_internal, mut rx_internal) = tokio::sync::mpsc::channel::<AgentLoopMsg>(32);
    let tx_forward = tx_internal.clone();

    tokio::spawn(async move {
        while let Some(op) = rx.recv().await {
            if tx_forward.send(AgentLoopMsg::External(op)).await.is_err() {
                break;
            }
        }
    });

    let mut pending_reply: Option<tokio::sync::oneshot::Sender<TurnOutcome>> = None;

    while let Some(msg) = rx_internal.recv().await {
        match msg {
            AgentLoopMsg::External(Op::UserTurn { text, reply }) => {
                if self.active_turn.is_some() {
                    if let Some(reply) = reply {
                        let _ = reply.send(TurnOutcome::Failed(
                            "agent is already running a turn".to_string(),
                        ));
                    }
                    continue;
                }

                self.session.push(Item::user(text));
                self.cancel = CancellationToken::new();
                self.active_turn = Some(ActiveTurn {
                    cancel: self.cancel.clone(),
                });
                pending_reply = reply;

                let outcome = self.run_turn().await;
                let _ = tx_internal.send(AgentLoopMsg::TurnFinished(outcome)).await;
            }
            AgentLoopMsg::External(Op::Interrupt) => {
                if let Some(active) = &self.active_turn {
                    active.cancel.cancel();
                } else {
                    self.cancel.cancel();
                }
            }
            AgentLoopMsg::External(Op::Shutdown) => {
                if let Some(active) = &self.active_turn {
                    active.cancel.cancel();
                }
                break;
            }
            AgentLoopMsg::TurnFinished(outcome) => {
                self.active_turn = None;
                if let Some(reply) = pending_reply.take() {
                    let _ = reply.send(outcome);
                }
            }
        }
    }
}
```

If this implementation does not compile because `run_turn().await` still blocks message handling, use `tokio::select!` inside `run_turn` stream consumption instead. The important invariant is: while a turn is active, `Op::Interrupt` must be able to cancel the same `CancellationToken` that was passed into `Model::stream()`.

- [ ] **Step 6: Prefer the simpler compiling variant if ownership blocks spawning**

If moving `run_turn()` into a separate task requires invasive ownership changes, keep `Agent` single-owned and change `run_turn()` to accept an interrupt receiver:

```rust
async fn run_turn_with_interrupts(
    &mut self,
    rx: &mut tokio::sync::mpsc::Receiver<Op>,
) -> TurnOutcome
```

Inside the model stream loop, replace `stream.next().await` with:

```rust
let result = tokio::select! {
    event = stream.next() => event,
    op = rx.recv() => {
        match op {
            Some(Op::Interrupt) => {
                self.cancel.cancel();
                self.bus.publish(Event::TurnInterrupted);
                return TurnOutcome::Interrupted;
            }
            Some(Op::Shutdown) => {
                self.cancel.cancel();
                return TurnOutcome::Interrupted;
            }
            Some(Op::UserTurn { reply, .. }) => {
                if let Some(reply) = reply {
                    let _ = reply.send(TurnOutcome::Failed(
                        "agent is already running a turn".to_string(),
                    ));
                }
                continue;
            }
            None => return TurnOutcome::Failed("op channel closed".to_string()),
        }
    }
};
```

This variant is acceptable for funcode's current scope and avoids `Arc<Mutex<Agent>>`.

- [ ] **Step 7: Run focused interrupt tests**

Run:

```bash
cargo test agent::tests::handle_interrupt_cancels_running_turn agent::tests::mid_stream_interrupt_skips_session_push
```

Expected: both tests pass.

- [ ] **Step 8: Run full agent tests**

Run:

```bash
cargo test agent
```

Expected: all agent tests pass.

- [ ] **Step 9: Commit**

```bash
git add src/agent.rs
git commit -m "feat: process interrupt while agent turn runs"
```

---

### Task 4: Add Minimal TUI/CLI Op Forwarding Surface

**Files:**
- Modify: `src/tui.rs`
- Test: add tests in `src/tui.rs` if the module can host unit tests without blocking stdin

- [ ] **Step 1: Define the TUI runner boundary**

Add to `src/tui.rs`:

```rust
//! TUI / CLI interaction module.
//!
//! Current phase keeps this as a minimal async boundary between terminal input and AgentHandle.
//! It sends user actions as `Op` and renders `Bus` events; it does not own Agent internals.
//!
//! 参考: /home/acer/project/rust_project/codex-main/codex-rs/tui/src/chatwidget/agent.rs

use crate::agent::{AgentHandle, AgentHandleError, TurnOutcome};
use crate::bus::{Event, ReceiveResult};

#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("agent error: {0}")]
    Agent(#[from] AgentHandleError),
}

pub struct Tui {
    agent: AgentHandle,
}

impl Tui {
    pub fn new(agent: AgentHandle) -> Self {
        Self { agent }
    }

    pub async fn submit_line(&self, line: String) -> Result<TurnOutcome, TuiError> {
        self.agent.user_turn(line).await.map_err(TuiError::from)
    }

    pub async fn interrupt(&self) -> Result<(), TuiError> {
        self.agent.interrupt().await.map_err(TuiError::from)
    }
}
```

- [ ] **Step 2: Add a pure event rendering helper**

Add:

```rust
pub fn render_event(event: &Event) -> Option<String> {
    match event {
        Event::TurnStarted => Some("[turn started]".to_string()),
        Event::TurnInterrupted => Some("[turn interrupted]".to_string()),
        Event::TurnComplete { .. } => Some("[turn complete]".to_string()),
        Event::TextDelta(delta) => Some(delta.clone()),
        Event::TextDone(_) => None,
        Event::ToolCallBegin { name, .. } => Some(format!("[tool: {name}]")),
        Event::ToolCallEnd { name, is_error, .. } => {
            let status = if *is_error { "failed" } else { "ok" };
            Some(format!("[tool: {name} {status}]"))
        }
        Event::ApprovalRequired { tool_name, .. } => {
            Some(format!("[approval required: {tool_name}]"))
        }
        Event::Error(message) => Some(format!("[error: {message}]")),
    }
}
```

- [ ] **Step 3: Add unit tests for rendering**

Add at bottom of `src/tui.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_text_delta_as_text() {
        assert_eq!(
            render_event(&Event::TextDelta("hello".to_string())),
            Some("hello".to_string())
        );
    }

    #[test]
    fn render_tool_begin_as_status_line() {
        assert_eq!(
            render_event(&Event::ToolCallBegin {
                id: "call_1".to_string(),
                name: "read_file".to_string(),
            }),
            Some("[tool: read_file]".to_string())
        );
    }
}
```

- [ ] **Step 4: Run TUI tests**

Run:

```bash
cargo test tui
```

Expected: TUI tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/tui.rs
git commit -m "feat: add tui agent handle boundary"
```

---

### Task 5: Wire App Assembly to Use AgentHandle

**Files:**
- Modify: `src/app.rs`
- Modify: `src/main.rs` only if needed for compilation after previous API changes
- Test: compile tests

- [ ] **Step 1: Add app-level constructor for an existing Agent**

Because real config/model assembly is still a separate milestone, add a minimal app wrapper that accepts an already-built `Agent`:

```rust
//! 应用装配与生命周期管理模块。
//!
//! Current phase exposes a small assembly boundary that turns an Agent domain object
//! into an AgentHandle for TUI/CLI code.

use crate::agent::{Agent, AgentHandle};

pub struct App {
    agent: AgentHandle,
}

impl App {
    pub fn from_agent(agent: Agent) -> Self {
        Self {
            agent: Agent::spawn(agent, 32),
        }
    }

    pub fn agent(&self) -> AgentHandle {
        self.agent.clone()
    }
}
```

- [ ] **Step 2: Add no-op-safe module test if test helpers are accessible**

If `Agent` test helpers are private and not reusable, only compile this module with full tests. Do not duplicate model mock types in `app.rs` unless needed.

Run:

```bash
cargo test app
```

Expected: compilation succeeds. It is acceptable if there are zero app tests.

- [ ] **Step 3: Run full test suite**

Run:

```bash
cargo test
```

Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/app.rs src/main.rs
git commit -m "feat: assemble app around agent handle"
```

---

### Task 6: Update Documentation for Queue-Based Agent Interaction

**Files:**
- Modify: `docs/2026-04-28-model-tool-loop-status-plan.md`
- Create: `docs/adr/0010-agent-op-queue.md`

- [ ] **Step 1: Create ADR for Agent op queue**

Create `docs/adr/0010-agent-op-queue.md`:

```markdown
# ADR-0010: Agent Op Queue and Handle Boundary

**Date**: 2026-04-28
**Status**: accepted
**Deciders**: project lead

## Context

`Agent::submit(&mut self, Op::UserTurn)` originally awaited the full model-tool loop. That made `Op::Interrupt` hard to use from TUI code because callers could not borrow the same Agent while a turn was running.

Codex CLI uses a queue-pair design: callers submit `Op` values into a queue, while UI code consumes observation events separately.

References:

- `/home/acer/project/rust_project/codex-main/codex-rs/core/src/codex.rs`
- `/home/acer/project/rust_project/codex-main/codex-rs/core/src/tasks/mod.rs`
- `/home/acer/project/rust_project/codex-main/codex-rs/tui/src/chatwidget/agent.rs`

## Decision

funcode introduces `AgentHandle` as the external boundary for TUI/app code. The handle sends `Op` values over a Tokio `mpsc` queue. `Bus` remains the Agent-to-UI observation channel.

The core direction is:

```text
TUI/App -> AgentHandle -> mpsc<Op> -> Agent loop -> Bus<Event> -> TUI/App
```

`TurnOutcome` is used for structured per-turn control-flow results. `Bus` events remain optimized for rendering and observation.

## Consequences

### Positive

- TUI no longer needs `&mut Agent`.
- Interrupt can be delivered while a turn is active.
- Future approval responses can use the same `Op` path.
- Agent state remains single-owned by the Agent loop.

### Negative

- Tests need to handle async queue behavior.
- Direct `Agent::submit()` remains useful for focused unit tests but should not be the primary app/TUI integration path.

## Future Work

- Add `Op::ApproveTool` and `Op::RejectTool`.
- Add cancellation-aware tool execution.
- Add a real config/app/TUI startup path.
```

- [ ] **Step 2: Update status plan**

In `docs/2026-04-28-model-tool-loop-status-plan.md`, update the section that says actor/op queue is deferred. Replace it with a note that the queue boundary is now planned or implemented depending on current code state:

```markdown
Agent/TUI communication is moving toward a queue-pair design: `AgentHandle` sends `Op` through `mpsc`, while `Bus` remains the event stream for UI rendering. This keeps the current model-tool loop intact while making interrupt and future approval flows possible.
```

- [ ] **Step 3: Run documentation sanity check**

Run:

```bash
grep -R "AgentHandle\|TurnOutcome\|Op Queue" docs/adr/0010-agent-op-queue.md docs/2026-04-28-model-tool-loop-status-plan.md
```

Expected: output includes all three concepts.

- [ ] **Step 4: Commit**

```bash
git add docs/adr/0010-agent-op-queue.md docs/2026-04-28-model-tool-loop-status-plan.md
git commit -m "docs: document agent op queue design"
```

---

## Verification Checklist

Run these after all tasks:

```bash
cargo test
```

Expected: all tests pass.

Manual behavior to verify after app/config/TUI exist:

```text
1. Start funcode.
2. Submit a prompt that causes a long model response.
3. Press Ctrl-C.
4. Observe Event::TurnInterrupted or equivalent UI message.
5. Submit another prompt.
6. Confirm the session still works.
```

## Self-Review

- Spec coverage: the plan covers structured turn outcomes, inbound op queue, interrupt delivery, minimal TUI boundary, app assembly boundary, and docs.
- Placeholder scan: no task depends on unspecified future behavior; shell/write/approval are explicitly out of scope.
- Type consistency: `AgentHandle`, `TurnOutcome`, `AgentHandleError`, `Op::user_turn`, and `Bus` cloning are introduced before later tasks use them.
- YAGNI check: no actor framework, no `Arc<Mutex<Agent>>`, no shell/write/approval implementation in this plan.
