# ADR-0003: Event bus with broadcast channel for Agent-CLI decoupling

**Date**: 2026-04-25
**Status**: accepted
**Deciders**: project lead

## Context

The Agent core loop needs to communicate real-time events (text deltas, tool call progress,
errors) to one or more consumers (CLI display, telemetry, logging). Direct coupling between
Agent and CLI would make testing difficult and prevent multiple simultaneous consumers.

Reference implementations:
- **Claude Code** uses callback functions and EventEmitter patterns.
- **Codex CLI** uses `tokio::broadcast` channels for event distribution.
- **OpenCode** uses a similar event streaming approach.

## Decision

We use `tokio::sync::broadcast` as the event bus backbone:

```rust
pub struct Bus {
    tx: broadcast::Sender<Event>,
}

pub struct Subscriber {
    rx: broadcast::Receiver<Event>,
}
```

The `Event` enum covers the full agent lifecycle: `TurnStarted`, `TextDelta`, `TextDone`,
`ToolCallBegin`, `ToolCallEnd`, `ApprovalRequired`, `Error`, `TurnComplete`.

Agent is the sole publisher; external modules call `bus.subscribe()` to get independent event
stream copies. Slow consumers receive a `Lagged(n)` notification and continue with the latest
events.

## Alternatives Considered

### Alternative 1: `mpsc` channel (single consumer)
- **Pros**: Simpler; no lag handling.
- **Cons**: Only one consumer; cannot serve CLI + telemetry simultaneously.
- **Why not**: Multiple consumers is a core requirement.

### Alternative 2: Callback / closure-based observers
- **Pros**: No async runtime dependency; synchronous.
- **Cons**: Callbacks cannot be async easily; registration lifecycle is complex; hard to test.
- **Why not**: The project is fully async; channels fit naturally.

### Alternative 3: `tokio::sync::watch` (single latest value)
- **Pros**: Minimal overhead; always holds latest value.
- **Cons**: Only stores the latest value; historical events are lost. Cannot represent streaming
  text deltas correctly.
- **Why not**: We need all events, not just the latest state.

## Consequences

### Positive
- Clean separation: Agent publishes events without knowing who consumes them.
- Multiple subscribers (CLI, telemetry, logging) without Agent changes.
- Backpressure via `Lagged` notification -- slow consumers don't block the Agent loop.
- Testable: tests subscribe to the bus and assert event sequences.

### Negative
- Slow consumers lose intermediate events (by design).
- Events must be `Clone`; large payloads (tool output) are cloned per subscriber.
- Broadcast channel capacity must be tuned; too small causes frequent lag, too large wastes
  memory.

### Risks
- If tool output is very large (e.g., reading a 10MB file), cloning per subscriber could be
  expensive. Mitigation: future ADR may introduce event truncation or shared references.
