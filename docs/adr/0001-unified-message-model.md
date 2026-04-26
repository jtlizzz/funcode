# ADR-0001: Unified message model with role-specific enum

**Date**: 2026-04-25
**Status**: accepted
**Deciders**: project lead

## Context

The coding assistant needs an internal message representation that maps to multiple LLM provider
formats (OpenAI, Anthropic, etc.). Different providers use different message shapes, and a naive
"one struct fits all" approach risks allowing invalid message combinations (e.g., a system message
with tool results, or a user message with tool call IDs).

Reference implementations handle this differently:
- **Claude Code** uses a flat `Message` type with optional fields.
- **Codex CLI** uses role-specific variants with typed payloads.
- **OpenCode** uses a similar variant-based approach.

## Decision

We use a role-specific Rust enum (`Message`) where each variant carries only the data valid for
that role:

```rust
pub enum Message {
    System(String),
    User(String),
    Assistant(Vec<AssistantBlock>),
    Tool { call: ToolCall, result: ToolResult },
}
```

`AssistantBlock` is also an enum (`Text | ToolCall`), allowing interleaved text and tool calls
within a single assistant turn -- matching the streaming behavior of OpenAI's chat completions.

Each variant provides factory methods (`Message::user()`, `Message::assistant_text()`, etc.) and
a `text_content()` accessor for extracting plain text.

## Alternatives Considered

### Alternative 1: Flat struct with optional fields
```rust
struct Message {
    role: Role,
    content: Option<String>,
    tool_calls: Option<Vec<ToolCall>>,
    tool_call_id: Option<String>,
}
```
- **Pros**: Simpler serialization; maps directly to JSON payloads.
- **Cons**: Allows invalid combinations (e.g., `role: System` + `tool_calls: Some(...)`). Requires
  runtime validation everywhere.
- **Why not**: Rust's type system can enforce correctness at compile time; we should use it.

### Alternative 2: Separate types per role with trait objects
- **Pros**: Maximum type safety; each role is its own type.
- **Cons**: Cannot store in a single `Vec`; requires `Box<dyn Any>` or an enum wrapper anyway.
- **Why not**: The enum already provides type safety; separate types add complexity without benefit.

## Consequences

### Positive
- Compile-time enforcement: impossible to construct a `User` message with tool calls.
- Pattern matching exhaustiveness ensures all message types are handled.
- `Assistant(Vec<AssistantBlock>)` naturally supports mixed text + tool call content.
- Easy to add `text_content()` that works across all variants.

### Negative
- Each provider adapter must pattern-match and convert; no auto-serialization.
- Adding a new message variant is a breaking change requiring updates to all match arms.

### Risks
- If a provider introduces a new role (e.g., "developer" in OpenAI's API), we need to add a new
  variant or map it to an existing one.
