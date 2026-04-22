# ADR-0006: Heuristic token budget truncation with last-user preservation

**Date**: 2026-04-25
**Status**: accepted
**Deciders**: project lead

## Context

LLM APIs have context window limits (e.g., 128K tokens for GPT-4o). As a conversation grows,
the total token count in the message history can exceed this limit. When this happens, the
assistant must trim older messages to fit within the budget.

Reference implementations:
- **Claude Code** uses `autoCompact.ts`: when token usage exceeds `contextWindow - 13000` (buffer),
  it summarizes old messages and replaces them with a compacted summary.
- **Codex CLI** uses `approx_token_count()` with a `BYTES_PER_TOKEN = 4` heuristic for estimation,
  plus a context manager that drops oldest messages.
- **OpenCode** uses a similar truncation strategy.

## Decision

We use a heuristic estimation (`text_content().len() / 4` bytes per token, matching Codex CLI)
and truncation from the oldest message:

```rust
pub fn truncate_to_budget(&mut self) {
    while self.estimate_tokens() > budget && self.messages.len() > 1 {
        self.messages.remove(0);
    }
}
```

Key design choices:
1. **Heuristic estimation** instead of exact tokenization -- avoids provider-specific tokenizers.
2. **Oldest-first removal** -- preserves the most recent context.
3. **Last user message preserved** -- the agent always knows what the user asked, even after
   aggressive truncation.
4. **System prompt excluded from budget** -- it is always prepended by `build_request()` and
   managed by the provider.
5. Called at the start of each `run_turn()` iteration, before building the request.

## Alternatives Considered

### Alternative 1: Exact tokenization with tiktoken
- **Pros**: Precise token counts; no over/under-truncation.
- **Cons**: Requires `tiktoken` Rust bindings (heavy dependency); tokenizer varies by model.
- **Why not**: Heuristic is sufficient for truncation (safety margin covers estimation error).

### Alternative 2: Summarization (compaction) instead of truncation
- **Pros**: Preserves key information from old messages.
- **Cons**: Requires an extra LLM call; summarization quality varies; adds latency.
- **Why not**: Planned for Phase 2. Phase 1 uses truncation as the simple, predictable baseline.

### Alternative 3: Priority-based retention
- **Pros**: Keep the most "important" messages instead of just the most recent.
- **Cons**: Defining "importance" is complex; requires scoring heuristics.
- **Why not**: Over-engineered for Phase 1. Recent-first with last-user preservation is a good
  default.

## Consequences

### Positive
- Simple, predictable behavior: oldest messages are dropped first.
- No external dependencies for tokenization.
- Last-user preservation ensures the agent never "forgets" the current question.
- System prompt is never truncated.

### Negative
- Heuristic estimation may over-count (wasting context) or under-count (risking API rejection).
- Important early context (e.g., initial instructions) may be lost in long conversations.
- `messages.remove(0)` is O(n) for `Vec`; inefficient for very long histories.

### Risks
- If the heuristic significantly under-estimates tokens, the API may reject the request with a
  context-length error. Mitigation: apply a safety buffer (e.g., set `max_context_tokens` to
  `model_window - 13000`).
- O(n) removal can be fixed by switching to `VecDeque` if needed.
