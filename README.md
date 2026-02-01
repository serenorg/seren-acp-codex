# seren-acp-codex

ACP (Agent Client Protocol) for OpenAI Codex.

## What it is

`seren-acp-codex` speaks ACP JSON-RPC over stdio and internally:
- Spawns `codex app-server` as a subprocess
- Translates ACP prompt turns to Codex app-server requests
- Streams Codex events back to the client as ACP session updates
- Bridges Codex approval prompts through ACP `session/request_permission`

## Architecture

```
┌────────────┐     stdio      ┌──────────────────┐     stdio      ┌──────────────────┐
│ ACP Client │ ─────────────► │ seren-acp-codex  │ ─────────────► │ codex app-server │
│            │ ◄───────────── │                  │ ◄───────────── │ (subprocess)     │
└────────────┘   JSON-RPC     └──────────────────┘   JSON-RPC     └──────────────────┘
```

## Prerequisites

- Install Codex CLI (see the Codex CLI docs: https://developers.openai.com/codex/cli/)
- Authenticate with Codex: `codex login`
- ACP spec: https://agentclientprotocol.com/

## Running

Note: this binary expects an ACP client to drive it over stdin/stdout. Running it directly will wait for JSON-RPC messages.

```bash
# Debug build
cargo run --bin seren-acp-codex

# Release build
cargo run --release --bin seren-acp-codex

# Enable logs
RUST_LOG=info cargo run --release --bin seren-acp-codex
RUST_LOG=debug cargo run --release --bin seren-acp-codex
```

## Session modes

| Mode ID | Meaning |
|--------:|---------|
| `ask` | Ask before running tools (default) |
| `auto` | Auto-approve safe operations |

## ACP support

**Methods**
- `initialize`
- `authenticate` (no-op; Codex CLI handles auth)
- `newSession`
- `prompt`
- `setSessionMode`
- `cancel`
- `loadSession` is not supported

**High-level event mapping**
- ACP session = Codex thread
- ACP tool calls / permission prompts = Codex approvals (command execution / file changes)

## Development

- MSRV: Rust `1.85` (see `Cargo.toml`)
- `cargo fmt`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test`

## License

MIT
