# ADR-0005: Stateless session with full-history rebuild

**Date**: 2026-04-25
**Status**: accepted
**Deciders**: project lead

## Context

The coding assistant maintains a conversation with the LLM across multiple turns. Each turn may
include user input, assistant responses, and tool call/result pairs. The system must track this
history and present it to the model in each API call.

Reference implementations:
- **Claude Code** maintains full message history and uses `autoCompact` to compress when
  approaching token limits.
- **Codex CLI** uses a similar full-history approach with a context manager.
- **OpenCode** rebuilds the request from scratch each turn.

## Decision

`Session` is a stateless accumulator: it holds `system_prompt` and `messages: Vec<Message>`, and
`build_request()` assembles the full request fresh each call:

```
[Message::System(system_prompt), ...self.messages, tools]
```

There is no server-side session ID or incremental update mechanism. Every API call contains the
complete conversation history.

Token usage is tracked cumulatively via `record_usage()` from API response `usage` fields. When
the estimated token count exceeds `max_context_tokens`, `truncate_to_budget()` removes the oldest
messages.

## Alternatives Considered

### Alternative 1: Server-side session (session ID)
- **Pros**: Smaller request payloads; server manages context.
- **Cons**: Most LLM APIs don't support server-side sessions; would require custom proxy.
- **Why not**: No standard API supports this; adds complexity without provider support.

### Alternative 2: Incremental message append (delta-based)
- **Pros**: Only new messages sent per turn; lower bandwidth.
- **Cons**: Provider APIs expect full message history; would need client-side state tracking
  for delta computation.
- **Why not**: All target APIs require full history; delta tracking adds complexity for no gain.

### Alternative 3: Sliding window with summarization
- **Pros**: Proactive context management; always fits within budget.
- **Cons**: Summarization requires an extra LLM call; may lose important context.
- **Why not**: Phase 1 uses simple truncation. Summarization (compaction) is planned for Phase 2.

## Consequences

### Positive
- Simple implementation: no delta tracking, no session synchronization.
- Works with any provider API out of the box.
- `build_request()` is idempotent -- can be called multiple times safely.
- `clear()` enables session reset without recreating the `Session` object.

### Negative
- Full history sent every turn; bandwidth grows linearly with conversation length.
- Large conversations approach token limits, requiring truncation (which loses old context).

### Risks
- Truncation may remove important context from early turns. Mitigation: Phase 2 will add
  summarization (compaction) to preserve key information from truncated messages.
