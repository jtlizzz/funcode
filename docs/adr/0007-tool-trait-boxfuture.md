# ADR-0007: Tool trait with `#[async_trait]`

**Date**: 2026-04-25
**Status**: accepted
**Deciders**: project lead

## Context

The tool system must support asynchronous execution (shell commands, file I/O, HTTP requests)
while allowing dynamic registration and dispatch. Tools are registered at runtime, so the
concrete tool type is erased and accessed via `dyn Tool`.

Reference implementations:
- **Claude Code** uses a JavaScript-based tool system with async functions.
- **Codex CLI** defines a tool trait with async execution and uses dynamic dispatch.
- **OpenCode** uses a similar trait-based approach.

## Decision

We use `#[async_trait]` on the `Tool` trait, allowing implementors to write `async fn` directly:

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> Value;
    async fn execute(&self, arguments: &str) -> Result<String, ToolError>;
}
```

Under the hood, `#[async_trait]` generates `Pin<Box<dyn Future + Send>>` — the same boxing that
a manual `BoxFuture` would require. Since `ToolRegistry` stores `Box<dyn Tool>` (dynamic dispatch),
boxing is mandatory regardless of syntax choice (RPITIT does not support `dyn`).

The project already depends on `async-trait` for `ModelProvider`, so this does not add a new
dependency. Using the same pattern across both core traits keeps the codebase consistent.

`ToolRegistry` stores tools as `HashMap<String, Box<dyn Tool>>` and routes calls by name.
Execution results are normalized to `ToolResult` (success or error), so tool failures never
crash the agent loop.

## Alternatives Considered

### Alternative 1: Manual `BoxFuture` return type
```rust
type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

pub trait Tool: Send + Sync {
    fn execute(&self, arguments: &str) -> BoxFuture<Result<String, ToolError>>;
}
```
- **Pros**: Explicit about the allocation; no proc-macro involved in the trait definition.
- **Cons**: Implementors must write `Box::pin(async move { ... })` boilerplate. Closure captures
  force cloning borrowed arguments (e.g., `let args = arguments.to_string()`). Inconsistent with
  `ModelProvider` which already uses `#[async_trait]`.
- **Why not**: The "explicit allocation" argument is moot when the project already uses
  `#[async_trait]` for `ModelProvider`. The generated code is identical. The real cost is
  implementor friction and inconsistency.

### Alternative 2: Synchronous trait with `spawn_blocking`
- **Pros**: Simpler trait signature; no async in trait.
- **Cons**: Shell execution and HTTP calls are naturally async; `spawn_blocking` wastes thread
  pool resources on I/O-bound work.
- **Why not**: Tools are I/O-bound; async is the right model.

### Alternative 3: Enum-based tool dispatch
```rust
enum ToolAction { Shell(ShellTool), Fs(FsTool), ... }
```
- **Pros**: No allocation; exhaustive matching.
- **Cons**: Cannot add custom tools without modifying the enum.
- **Why not**: Plugin extensibility is a core requirement; trait objects allow third-party tools.

### Alternative 4: RPITIT (`impl Future` in trait)
```rust
pub trait Tool: Send + Sync {
    fn execute(&self, arguments: &str) -> impl Future<Output = Result<String, ToolError>> + Send;
}
```
- **Pros**: Zero-overhead; no boxing; stable since Rust 1.75.
- **Cons**: Not object-safe — cannot use with `dyn Tool`. `ToolRegistry` requires dynamic dispatch.
- **Why not**: `Box<dyn Tool>` is a hard requirement for runtime registration.

## Consequences

### Positive
- Object-safe trait: can be stored as `dyn Tool` in the registry.
- Implementors write natural `async fn` syntax — no `Box::pin` or manual clone workarounds.
- Consistent with `ModelProvider` trait pattern across the codebase.
- Dynamic registration: tools are added at runtime without recompilation.
- Tool failures are caught and converted to `ToolResult::error`; the agent loop never panics
  from a tool execution failure.
- `ToolSpec` generation is automatic via the default `spec()` method.

### Negative
- One heap allocation per `execute()` call (the boxed future) — same as any approach supporting
  `dyn` dispatch.
- `#[async_trait]` introduces `'async_trait` lifetime bounds in the expanded code, which can
  produce confusing compiler errors in edge cases.
- No compile-time checking of tool argument schemas — arguments are passed as raw JSON strings.

### Risks
- If a tool panics during execution, it could crash the agent. Mitigation: wrap tool execution
  in `catch_unwind` or use `AssertUnwindSafe` in the registry (not yet implemented).
- Raw JSON string arguments mean schema validation happens inside each tool, not at the registry
  level. Could be improved with a centralized validation layer.
