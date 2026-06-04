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

Six-crate Cargo workspace under `crates/` (a WinDbg port adds two more — see below). The
seam is **compiler-enforced**: `mcp-tools`/`mcp-session` depend only on the neutral
`debugger-core` and cannot name a DAP, lldb, or DbgEng type.

| Crate | Responsibility |
|-------|----------------|
| `debugger-core` | `DebuggerBackend` + `BackendFactory` traits, `BackendCapabilities`, neutral types, `BackendError`, `BackendEvent` — no tokio/rmcp/DAP |
| `dap-client` | generic DAP transport: Content-Length framing, seq/pending correlation, read-loop, stop-waiter |
| `lldb-backend` | `LldbBackend`/`LldbFactory`: lldb-dap detect/spawn, the launch/attach handshake, op→neutral translation |
| `mcp-session` | state machine (incl. the `is_dump` flag), breakpoint tracking, output buffer, frame-map cache, the `BackendEvent` event-pump |
| `mcp-tools` | the MCP tool handlers, `BackendRegistry` (runtime backend switcher), `Args` accessor, response/format/flatten helpers, the rmcp server |
| `debug-mcp` | the binary: builds a `BackendRegistry`, registers the platform's backend (`LldbFactory` under `cfg(not(windows))`; `WinDbgFactory` under `cfg(windows)` once it lands), serves stdio |

`crates/integration-tests/` holds the live-suite harness (dev-dependency only, so the seam stays intact).

**Backends are platform-exclusive.** lldb (lldb-dap) is the **macOS/Linux** backend; WinDbg
is the **Windows** backend. lldb-on-Windows is deferred (a later addition). The binary
registers `LldbFactory` only under `cfg(not(windows))` and `WinDbgFactory` only under
`cfg(windows)`. Each backend's platform-bound tests run only on that platform:
`lldb-backend/tests/subprocess.rs` and the `integration`-feature suites are Unix-gated
(`cfg(unix)` / `cfg(all(feature = "integration", unix))`); WinDbg tests are `cfg(windows)` +
the `integration-windbg` feature. lldb's pure DAP-logic tests (duplex fakes, `FakeEnv`) stay
cross-platform (free compile/behavior coverage).

**Backend selection (runtime switcher).** `ToolServer` holds a `BackendRegistry` instead of
a single factory; the switcher is retained even though one backend ships per OS, so
lldb-on-Windows is a one-line additive registration later. The connect points pick a factory
per call: `launch`/`attach` honor an optional `backend` arg (`"lldb"`/`"windbg"`), then
`DEBUG_BACKEND`, then the per-OS default (`windbg` on Windows, `lldb` elsewhere);
`open_crash_dump`/`attach_kernel` force-select `windbg`. The advertised tool list is
capability-gated: the 21 base tools always, plus the four WinDbg-only tools (`open_crash_dump`,
`attach_kernel`, `analyze_crash`, `get_modules`) when a registered factory's
`BackendCapabilities` enables them (so non-Windows = exactly 21).

**WinDbg port (in progress; see `.plans/{Designs,Plans}/WinDbgBackend`).** Adds two
`cfg(windows)` crates *below* the seam: `dbgeng-sys` (the **only** crate with `unsafe` — all
DbgEng COM/FFI confined behind a safe synchronous `Engine`, built on the `windows` crate) and
`windbg-backend` (`#![forbid(unsafe_code)]`: a dedicated engine thread + `WinDbgBackend`/
`WinDbgFactory`). Nothing above the seam changes.

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
- **No `#[allow(...)]`** — fix clippy/compiler warnings at the source.
  Gate: `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- **`unsafe` is confined to one crate.** Every crate is `#![forbid(unsafe_code)]` except
  `dbgeng-sys` (the WinDbg COM/FFI layer), where `unsafe` is unavoidable but isolated and each
  block carries a `// SAFETY:` comment. This is a deliberate, documented deviation from the
  original "target zero `unsafe`" goal — a CI grep gate asserts `unsafe` appears only under
  `crates/dbgeng-sys/src/`. (`dbgeng-sys` does not exist yet; the rule applies as it lands.)

## Parity notes (vs the Go oracle)

Two intentional deviations from the Go server (everything else is strict parity): the
server identity rename (`lldb-debug-mcp`→`debug-mcp`, MCP server name `lldb-debug`→`debug`),
and `disassemble` default `instruction_count = 20` (Go code used 10). The DAP-handshake
`clientID` sent to lldb-dap stays `lldb-debug-mcp` (below the seam). This lldb-dap version
defers the launch/attach response until after `configurationDone`, so the handshake gates
configuration on the `InitializedEvent`, not the response.

## WinDbg-era additions (post-Go, deliberate extensions)

These extend the frozen Go-parity surface to support the pluggable WinDbg backend; the 21
lldb tools stay byte-identical:

- **Optional `backend` arg** on `launch`/`attach` (enum `["lldb","windbg"]`); additive,
  not required, so existing calls are unchanged. `status` gains additive `backend` (the
  active backend, recorded at connect; falls back to the per-OS default pre-connect) and
  `available_backends` fields.
- **Four capability-gated tools** — `open_crash_dump`, `attach_kernel`, `analyze_crash`,
  `get_modules` — advertised only when a WinDbg-capable factory is registered; they return a
  clear "not available"/`Unsupported` tool-error otherwise. `run_command` covers WinDbg's
  `execute_command` (raw command escape hatch).
- **Crash-dump sessions** are `State::Stopped` with an `is_dump` flag; `continue`/`step_*`
  reject them with the frozen literal `"cannot continue a crash-dump session"`.
