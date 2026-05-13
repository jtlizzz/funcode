# Tool Approval Design

## Problem

Tools execute immediately without user confirmation. A model response could trigger `rm -rf` via BashTool. We need an approval gate: the agent blocks, the TUI shows a prompt, the user decides.

## Reference

| Project | Approach | Key Pattern |
|---------|----------|-------------|
| Codex CLI | Policy engine + Guardian AI + oneshot channel | Overkill for V1 |
| DeepSeek-TUI | Tool category map + oneshot channel + session cache | Right model for funcode |

Both share the same core mechanism: agent creates a `oneshot::channel`, blocks on `rx.await`, TUI sends decision through `tx`.

## Architecture

```
execute_tools(call)
  ├─ registry.needs_approval(&call.name)?
  │   ├─ false → execute directly
  │   └─ true  → create oneshot channel
  │             → emit Event::ApprovalRequired { ..., responder: tx }
  │             → await rx  ◄── agent task blocks here
  │   Approved         → execute tool
  │   ApprovedForSession → execute + cache key in TUI
  │   Denied           → push error ToolResult
  │   Abort            → push error ToolResult + cancel token
  └─ (next call in loop)
```

Bus uses `mpsc` (not broadcast). Event does not need `Clone` — the `oneshot::Sender` goes directly into the event. TUI receives the event, takes the sender, and sends the decision back through it. No shared state, no HashMap workaround.

## Changes (4 files)

### 1. `src/tools.rs` — Tool trait: add `needs_approval()`

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    // ... existing methods ...

    /// Whether this tool requires user approval before execution.
    /// Default: `true` (conservative — only safe read-only tools override).
    fn needs_approval(&self) -> bool {
        true
    }
}
```

Override in read-only tools:
- `FileReadTool` → `false`
- `GlobTool` → `false`
- `BashTool`, `FileEditTool`, `FileWriteTool` → keep default `true`

Add to `ToolRegistry`:
```rust
pub fn needs_approval(&self, name: &str) -> bool {
    self.tools.get(name).map_or(true, |t| t.needs_approval())
}
```

### 2. `src/bus.rs` — Bus switched to mpsc (done separately)

Bus already uses `mpsc` instead of `broadcast`. Event does not derive `Clone`.

### 3. `src/agent.rs` — Approval gate in execute_tools

New type:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum ReviewDecision {
    Approved,
    ApprovedForSession,
    Denied,
    Abort,
}
```

In `execute_tools`, per-call approval check — oneshot sender goes directly in Event:

```rust
for call in calls {
    if self.registry.needs_approval(&call.name) {
        let (tx, rx) = tokio::sync::oneshot::channel();

        self.bus.publish(Event::ApprovalRequired {
            id: call.id.clone(),
            tool_name: call.name.clone(),
            description: call.name.clone(),   // V1: tool name as description
            arguments: call.arguments.clone(),
            responder: tx,                     // ← directly in event, no shared state
        });

        match rx.await {
            Ok(ReviewDecision::Approved)
            | Ok(ReviewDecision::ApprovedForSession) => { /* proceed */ }
            Ok(ReviewDecision::Denied) => {
                results.push(ToolResult::error(call.id, &call.name, "denied by user"));
                continue;
            }
            Ok(ReviewDecision::Abort) => {
                results.push(ToolResult::error(call.id, &call.name, "aborted by user"));
                self.cancel_current_turn();
                return results;
            }
            Err(_) => {
                results.push(ToolResult::error(call.id, &call.name, "approval cancelled"));
                return results;
            }
        }
    }

    // Existing execution
    let result = self.registry.execute_with_context(...).await;
    results.push(result);
}
```

### 4. `src/tui.rs` — Approval modal

Add to `DisplayState`:

```rust
pending_approval: Option<PendingApproval>,   // None = no modal

struct PendingApproval {
    id: String,
    tool_name: String,
    arguments: String,
    responder: oneshot::Sender<ReviewDecision>,   // ← take sender from event
}
```

Add to `TuiAction`:

```rust
RespondApproval(String, ReviewDecision),
```

In `handle_agent_event`, on `Event::ApprovalRequired`:
- Check session cache first (`HashSet<String>` of approved `"tool_name:args_hash"` keys)
- If cached: send Approved via responder immediately, don't show modal
- If not cached: store responder in `pending_approval`, TUI enters approval mode

In `handle_key_event`, when `pending_approval.is_some()`:
- Intercept **all** keys (no input editing during approval)
- `y` → send Approved via responder
- `a` → send ApprovedForSession via responder, add key to session cache
- `n` / `Escape` → send Denied via responder
- Clear `pending_approval` after sending

In `render`, when `pending_approval.is_some()`:
- Draw a highlighted prompt line at the bottom of the output area:
  ```
  [bash] {"command":"rm -rf /tmp"}  [y]es [a]lways [n]o
  ```

## What V1 Does NOT Include

- No policy engine / `.rules` files (Codex-style)
- No tool category enum (just `needs_approval()` bool per tool)
- No risk-based double-confirmation (DeepSeek-style) — single keypress for all
- No `description` field on each tool for richer prompt text (V1 uses tool name)
- No persistent approval config across sessions
- No parallel tool approval (tools execute serially already)

## Implementation Order

1. `bus.rs` — Switch from broadcast to mpsc (prerequisite, done separately)
2. `tools.rs` — Add `needs_approval()` to trait + registry
3. `agent.rs` — Add `ReviewDecision`, approval gate in `execute_tools`
4. `tui.rs` — Add approval modal state, key handling, rendering, cache
5. Tests — Update existing agent tests that hit tool execution
