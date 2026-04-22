# ADR-0004: Unified Op enum as Agent entry point

**Date**: 2026-04-25
**Status**: accepted
**Deciders**: project lead

## Context

The Agent needs to handle multiple user actions: sending text, interrupting generation, and
potentially switching models or canceling tool execution. These actions come from different UI
paths but must be processed consistently by the Agent loop.

Reference implementations:
- **Claude Code** uses a dispersed approach: `AbortController` for interrupts, direct state
  mutation for model switching.
- **Codex CLI** uses a unified `Op` enum where all user actions flow through a single `submit()`
  entry point.

## Decision

We adopt Codex CLI's unified `Op` enum pattern:

```rust
pub enum Op {
    UserTurn(String),
    Interrupt,
}
```

All external interactions with `Agent` go through a single `submit(&mut self, op: Op)` method.
Phase 1 includes two variants; future variants (`SwitchModel`, `CancelTool`) can be added
without changing the call signature.

Interrupt uses a turn-scoped `tokio_util::sync::CancellationToken` so the Agent can propagate
explicit cancellation into the model streaming path.

## Alternatives Considered

### Alternative 1: Separate methods (Claude Code style)
```rust
impl Agent {
    async fn send_message(&mut self, text: String);
    fn interrupt(&self);
    fn switch_model(&mut self, model: String);
}
```
- **Pros**: Each method is self-documenting; no match boilerplate.
- **Cons**: State management is dispersed; race conditions between methods are harder to reason
  about. No single serialization point.
- **Why not**: A single entry point provides a natural serialization boundary and makes state
  transitions explicit.

### Alternative 2: Message-passing via channel
```rust
tx.send(AgentCommand::UserTurn(text)).await;
```
- **Pros**: Fully decoupled; Agent runs in its own task.
- **Cons**: Requires a dedicated Agent task; response handling becomes complex (needs reply
  channels). Overkill for single-user CLI.
- **Why not**: Phase 1 is single-user; `&mut self` is sufficient and simpler.

## Consequences

### Positive
- Single entry point makes state transitions explicit and serializable.
- Easy to add new operations without changing the external API.
- Interrupt signal is non-blocking and propagates naturally through async call chains.
- `CancellationToken` is the ecosystem-standard primitive for cooperative async cancellation.

### Negative
- `submit()` is `&mut self`, so only one operation at a time. This blocks concurrent
  submit/interrupt from different tasks (acceptable for single-user CLI).

### Risks
- If we need true concurrent access (e.g., background tasks submitting to the same Agent), we
  will need to switch to a channel-based approach or wrap Agent in `Arc<Mutex<Agent>>`.
