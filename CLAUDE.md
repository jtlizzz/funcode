# funcode

A terminal AI programming assistant written in Rust.

## Architecture Style: Domain-Driven + Object-Oriented

- **Domain objects first**: Model, Session, Tool, and Agent are domain objects with clear responsibilities, not thin wrappers
- **Domain objects own core logic**: Model is responsible for full message assembly; Agent only consumes domain events without redundant accumulation
- **Single-file modules**: Each `.rs` file maps to a complete functional domain, no subdirectory splitting. File = module boundary, naming = functional boundary
- **Domain events vs observation events**:
  - Domain events (e.g. `ItemsDone`) are the authoritative data source, carrying complete results
  - Observation events (e.g. `TextDelta`, `ToolCallStart`) are real-time feedback for UI consumption

## Layered Responsibilities

```
Agent (orchestrator) → Uses Model for responses, Tool for execution, Session for history, Bus for notification
Model (domain object) → Owns message assembly logic, returns complete messages
ModelProvider         → Protocol adapter (SSE parsing, API format conversion), pure technical detail
Session (domain obj)  → Owns message history, token budget, truncation logic
Tool (domain object)  → Owns tool definitions and execution logic
Bus (infrastructure)  → Event broadcasting, connects Agent and UI
```

## Reference Projects

| Project | Language | Local Path | Core Directory |
|---------|----------|------------|----------------|
| Claude Code | TypeScript | `/home/acer/project/node_project/claude-code` | `src/` |
| Codex CLI | Rust | `/home/acer/project/rust_project/codex-main` | `codex-rs/` |
| OpenCode | TypeScript | `/home/acer/project/node_project/opencode` | `packages/opencode/src/` |

## Project Documentation

- **Implementation Roadmap**: `/mnt/c/Users/acer/Documents/tech/funcode - 实现路线图.md`

## Constraints

- Recommended reading priority: Claude Code > Codex CLI > OpenCode
- Reference corresponding files from reference projects as design basis (annotate with `参考:` + file path in comments)
- Use `// === Section Name ===` comments to divide regions within files
- **Avoid premature abstraction**: Prefer keeping logic local to the owning domain/module until at least 2-3 real call sites or a stable cross-domain concept appears. Do not create standalone `utils`/helper modules for one-off logic that is easier to understand inline.
- **Don't avoid introducing dependencies**: Prefer mature crates from the ecosystem (e.g. `tokio_util::sync::CancellationToken`) over suboptimal solutions just to minimize dependencies. Refactoring cost is not a concern — if a better approach requires changes in multiple places, just say so
