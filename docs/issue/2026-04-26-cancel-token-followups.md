# Cancel Token Follow-ups

Date: 2026-04-26
Related ADR: `docs/adr/0008-cancel-token-stream-assembly.md`

## Status

This note records the two cancellation issues identified during the `CancellationToken`
refactor and how they were resolved in the current implementation.

## Issue 1: EOF without authoritative terminal event looked like success

- Severity: High
- Status: Resolved

### Original problem

The old `consume_stream()` path could treat EOF as a normal stream end even when the provider
never emitted the authoritative terminal response event. That allowed an incomplete turn to look
successful.

### Resolution

The stream loop is now inlined in `Agent::run_turn()` and only considers the turn successful after
receiving:

```rust
ResponseEvent::Completed { usage, finish_reason }
```

Before `Completed`, the agent may also receive authoritative completed item events such as
`TextDone` and `ToolCallReady`.

If the stream ends before `Completed`, the agent now reports a protocol/runtime error instead of
publishing `TurnComplete`.

## Issue 2: Late cancellation could preempt an already buffered final response

- Severity: Medium
- Status: Resolved

### Original problem

The earlier design considered checking cancellation in `ResponseStream::poll_next()`. That would
allow consumer-side cancellation to hide already-buffered authoritative events if cancellation
happened just after the provider had produced them.

### Resolution

The final design does **not** let `ResponseStream` inspect the cancellation token directly.
Instead:

- provider-side stream tasks observe `CancellationToken`
- provider emits explicit `ResponseEvent::Cancelled`
- `ResponseStream` simply drains its event channel

This keeps already-buffered terminal events observable by the agent while still allowing upstream
work to stop promptly.

## Remaining follow-up

Current cancellation only interrupts the model stream. It does **not** yet cancel an already
running tool execution. That follow-up remains intentionally out of scope for this change set.
