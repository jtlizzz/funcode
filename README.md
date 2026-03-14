# FunCode

A powerful AI agent framework built with Rust.

## Project Structure

```
funcode/
├── Cargo.toml          # Workspace configuration
├── crates/
│   ├── core/           # Core agent, session, tool, permission, and event models
│   ├── provider/       # LLM provider implementations
│   ├── lsp/            # Language Server Protocol client
│   ├── mcp/            # Model Context Protocol transport
│   ├── tools/          # Built-in tools (filesystem, shell)
│   ├── server/         # HTTP API server
│   ├── db/             # Database persistence layer
│   ├── tui/            # Terminal user interface
│   └── cli/            # Command-line interface
└── README.md
```

## Building

```bash
cargo build --release
```

## Running

```bash
cargo run --bin funcode
```

## Features

- **Multi-Agent System**: Support for multiple concurrent agents
- **Tool System**: Extensible tool architecture with built-in filesystem and shell tools
- **LSP Integration**: Full Language Server Protocol support
- **MCP Support**: Model Context Protocol for advanced AI interactions
- **HTTP API**: RESTful API for external integrations
- **SQLite Database**: Persistent storage for sessions and configurations
- **TUI Interface**: Interactive terminal user interface
- **CLI Tool**: Command-line interface for everyday use

## Development

```bash
# Run tests
cargo test

# Run with logging
RUST_LOG=debug cargo run --bin funcode

# Format code
cargo fmt

# Check for issues
cargo clippy
```

## License

MIT
