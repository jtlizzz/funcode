# ADR-0002: ModelProvider trait with dynamic dispatch

**Date**: 2026-04-25
**Status**: accepted
**Deciders**: project lead

## Context

The assistant needs to support multiple LLM providers (OpenAI-compatible APIs, Anthropic, local
models, etc.) with a unified interface. The upper layer (Agent, Session) should not know which
provider is in use.

Reference implementations:
- **Claude Code** hardcodes the Anthropic SDK.
- **Codex CLI** uses a `ModelProvider` trait with dynamic dispatch via `Arc<dyn ModelProvider>`.
- **OpenCode** uses a provider interface with multiple implementations.

## Decision

We define a `ModelProvider` async trait with `send` (non-streaming) and `stream` methods:

```rust
#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn send(&self, model: &str, request: ModelRequest) -> Result<ModelResponse, ModelError>;
    async fn stream(&self, model: &str, request: ModelRequest) -> Result<ResponseStream, ModelError>;
}
```

The `Model` struct wraps `Box<dyn ModelProvider>` and delegates calls:

```rust
pub struct Model {
    provider: Box<dyn ModelProvider>,
    model: String,
}
```

The first concrete implementation is `OpenAIProvider`, backed by the `async-openai` crate.

## Alternatives Considered

### Alternative 1: Generic parameter (`Model<P: ModelProvider>`)
- **Pros**: No dynamic dispatch overhead; monomorphization enables inlining.
- **Cons**: `Agent` and all upstream types must carry the generic parameter -- viral generics.
- **Why not**: The trait object overhead is negligible for LLM API calls (network-bound). Avoiding
  viral generics is worth the minimal overhead.

### Alternative 2: Enum-based provider selection
```rust
enum Provider { OpenAI(OpenAIProvider), Anthropic(AnthropicProvider) }
```
- **Pros**: No allocation; exhaustive matching.
- **Cons**: Adding a provider requires modifying the enum; not extensible by users.
- **Why not**: Trait objects allow third-party provider implementations without core changes.

### Alternative 3: Static dispatch with `impl Trait`
- **Pros**: Zero overhead.
- **Cons**: Cannot store in structs; cannot swap providers at runtime.
- **Why not**: We need runtime provider selection (user may configure API key, base URL, etc.).

## Consequences

### Positive
- Adding a new provider only requires implementing `ModelProvider`; no changes to `Model`, `Agent`,
  or `Session`.
- `Model` can be swapped at runtime (e.g., user changes model mid-session).
- The `async-openai` crate handles SSE streaming, retries, and error types for OpenAI-compatible
  APIs.

### Negative
- Virtual dispatch on every `send`/`stream` call (negligible for network I/O).
- `Box<dyn ModelProvider>` is not `Clone`; must be wrapped in `Arc` if shared across tasks.

### Risks
- Provider-specific features (e.g., Anthropic's extended thinking, OpenAI's structured outputs)
  may not map cleanly to the unified `ModelRequest`/`ModelResponse`. May need provider-specific
  extensions later.
