# CLAUDE.md

## Project: debug-mcp

MCP server that exposes interactive native debugging to AI agents through a **pluggable
debugger backend**. The current backend wraps `lldb-dap` via the Debug Adapter Protocol;
the architecture is built so a second backend (e.g. WinDbg) can be added without touching
the tool layer. Rust rewrite of the original Go `lldb-debug-mcp` (kept feature-identical;
see `.plans/{Specs,Designs,Plans}/RustPort`).

## Build

```bash
cargo build --workspace
cargo build --release -p debug-mcp   # the published binary: debug-mcp
```

## Test

```bash
# Unit tests (all crates)
cargo test --workspace

# Live integration + differential parity (requires lldb-dap + compiled fixtures)
make -C testdata
cargo test -p mcp-tools --features integration -- --test-threads=1

# Full gate (also: make clippy / fmt-check / seam / tsan)
make all
```

## Architecture

```
AI Agent <-stdio/MCP(rmcp)-> [debug-mcp]
   tool handlers -> session manager -> DebuggerBackend trait (the seam)
                                          -> lldb-backend -> dap-client -> lldb-dap -> target
```

Six-crate Cargo workspace under `crates/`. The seam is **compiler-enforced**:
`mcp-tools`/`mcp-session` depend only on the neutral `debugger-core` and cannot name a
DAP or lldb type.

| Crate | Responsibility |
|-------|----------------|
| `debugger-core` | `DebuggerBackend` + `BackendFactory` traits, neutral types, `BackendError`, `BackendEvent` — no tokio/rmcp/DAP |
| `dap-client` | generic DAP transport: Content-Length framing, seq/pending correlation, read-loop, stop-waiter |
| `lldb-backend` | `LldbBackend`/`LldbFactory`: lldb-dap detect/spawn, the launch/attach handshake, op→neutral translation |
| `mcp-session` | state machine, breakpoint tracking, output buffer, frame-map cache, the `BackendEvent` event-pump |
| `mcp-tools` | the 21 MCP tool handlers, `Args` accessor, response/format/flatten helpers, the rmcp server |
| `debug-mcp` | the binary: registers `LldbFactory`, serves stdio |

`crates/integration-tests/` holds the live-suite harness (dev-dependency only, so the seam stays intact).

## Code Conventions

- Tool handlers return `ToolOutcome` (`Json`/`Text`/`Error`); user errors are tool-error
  results (`is_error`), never transport errors. Validation goes through the `Args` accessor
  (reproduces the Go error strings).
- State guards: call `session.check_state(&[...])` first; the guard strings are parity-exact.
- The backend trait is **coarse + blocking**: `launch`/`attach`/`cont`/`step` return the next
  `StopOutcome`; all DAP quirks (InitializedEvent ordering, `--repl-mode`) live in `lldb-backend`.
- Cancellation is at the tool layer (`tokio::select!` on the request token); never hold the
  session lock across an `.await`.
- **Tests in dedicated `tests/`/`src/tests/` folders**, not inline `#[cfg(test)]` modules.
- **No `#[allow(...)]`** — fix clippy/compiler warnings at the source. Target zero `unsafe`.
  Gate: `cargo clippy --workspace --all-targets --all-features -- -D warnings`.

## Parity notes (vs the Go oracle)

Two intentional deviations from the Go server (everything else is strict parity): the
server identity rename (`lldb-debug-mcp`→`debug-mcp`, MCP server name `lldb-debug`→`debug`),
and `disassemble` default `instruction_count = 20` (Go code used 10). The DAP-handshake
`clientID` sent to lldb-dap stays `lldb-debug-mcp` (below the seam). This lldb-dap version
defers the launch/attach response until after `configurationDone`, so the handshake gates
configuration on the `InitializedEvent`, not the response.
