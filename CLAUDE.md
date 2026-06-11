# CLAUDE.md

## Project: debug-mcp

MCP server that exposes interactive native debugging to AI agents through a **pluggable
debugger backend**. Two backends ship: `lldb-dap` (macOS/Linux) via the Debug Adapter
Protocol, and **WinDbg** (Windows) via DbgEng/COM — both behind the same neutral seam, added
without touching the tool layer. Rust rewrite of the original Go `lldb-debug-mcp` (kept
feature-identical; see `.plans/{Specs,Designs,Plans}/RustPort`).

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

# Live WinDbg integration (Windows only; requires the compiled fixture)
testdata\win\build.bat            # from a VS x64 Native Tools prompt (builds test_target.exe + .pdb)
make integration-windbg           # builds debug-mcp + runs the windbg suite single-threaded
# (each test self-skips when the fixture is absent; DbgEng is OS-bundled, so core debugging
#  needs no extra install; `analyze_crash` degrades gracefully without the full Debugging Tools.)

# Full gate (also: make clippy / fmt-check / seam / tsan)
make all
```

## Architecture

```
AI Agent <-stdio/MCP(rmcp)-> [debug-mcp]
   tool handlers -> session manager -> DebuggerBackend trait (the seam)
                                          -> lldb-backend -> dap-client -> lldb-dap -> target
```

Eight-crate Cargo workspace under `crates/` (six neutral/lldb crates + the two `cfg(windows)`
WinDbg crates below the seam). The seam is **compiler-enforced**: `mcp-tools`/`mcp-session`
depend only on the neutral `debugger-core` and cannot name a DAP, lldb, or DbgEng type.

| Crate | Responsibility |
|-------|----------------|
| `debugger-core` | `DebuggerBackend` + `BackendFactory` traits, `BackendCapabilities`, neutral types, `BackendError`, `BackendEvent` — no tokio/rmcp/DAP |
| `dap-client` | generic DAP transport: Content-Length framing, seq/pending correlation, read-loop, stop-waiter |
| `lldb-backend` | `LldbBackend`/`LldbFactory`: lldb-dap detect/spawn, the launch/attach handshake, op→neutral translation |
| `mcp-session` | state machine (incl. the `is_dump` flag), breakpoint tracking, output buffer, frame-map cache, the `BackendEvent` event-pump |
| `mcp-tools` | the MCP tool handlers, `BackendRegistry` (runtime backend switcher), `Args` accessor, response/format/flatten helpers, the rmcp server |
| `debug-mcp` | the binary: builds a `BackendRegistry`, registers the platform's backend (`LldbFactory` under `cfg(not(windows))`; `WinDbgFactory` under `cfg(windows)`), serves stdio |
| `dbgeng-sys` *(cfg(windows))* | the **only** crate with `unsafe`: all DbgEng COM/FFI confined behind a safe synchronous `Engine`, built on the `windows` crate |
| `windbg-backend` *(cfg(windows))* | `#![forbid(unsafe_code)]`: a dedicated engine thread + `WinDbgBackend`/`WinDbgFactory`; op→neutral translation below the seam |

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

**WinDbg port (shipped + parity-validated; see `.plans/{Designs,Plans}/WinDbgBackend`).** Two
`cfg(windows)` crates *below* the seam: `dbgeng-sys` (the **only** crate with `unsafe` — all
DbgEng COM/FFI confined behind a safe synchronous `Engine`, built on the `windows` crate) and
`windbg-backend` (`#![forbid(unsafe_code)]`: a dedicated engine thread + `WinDbgBackend`/
`WinDbgFactory`). The `WinDbgFactory` is registered by the binary under `cfg(windows)`, and the
live `integration-windbg` suite drives it against the OS-bundled DbgEng. Nothing above the seam
changes.

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
  original "target zero `unsafe`" goal — a CI grep gate (`make unsafe-gate`, also run on the
  Windows lane) asserts `unsafe` appears only under `crates/dbgeng-sys/src/`.

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

## WinDbg backend behavior notes (below the seam; frozen tool schema unchanged)

These are `windbg-backend`/`dbgeng-sys` behavior differences from the lldb backend. They live
*below* the seam — the frozen tool schemas and descriptions are byte-identical to the lldb
surface; only the runtime behavior differs (surfaced via result messages where it matters).

- **Address breakpoints (R6 / ASLR).** A bare `0x<address>` passed to `set_function_breakpoint`
  is **rejected** under WinDbg (an unverified result with guidance, not an engine breakpoint):
  `mcp-session` tracks function BPs **by name** and re-flushes them on every (re)launch, where a
  raw address would be ASLR-misplaced on the rebased image. `module!sym` (rebase-stable) is
  allowed and re-resolved via `GetOffsetByName` on each launch; for an address breakpoint use
  `run_command("bp <addr>")`. (Implementation: `windbg-backend/src/backend.rs`, the R6 rejection
  with `rejected:true`.)
- **`get_all_stacks` deviation.** The C++ plugin's `get_all_stacks` fast module-table /
  binary-search frame-resolution optimization was **not** ported; `stack_trace` resolves
  per-frame. A deliberate deviation, deferred unless per-frame latency proves material.
- **Extension path discovery (`!`-commands).** `analyze_crash` and `run_command("!...")` need the
  Debugging Tools for Windows extensions; the backend discovers the extension path at runtime
  (registry `KitsRoot10` → `WindowsSdkDir` → a built-in default). On a host without the extensions
  installed, `!analyze` returns the engine's `No export analyze found` (graceful degradation). The
  OS-bundled DbgEng (`System32`) drives **core** debugging without the full Tools installed.
- **Debuggee stdout is not surfaced** through `read_output` under WinDbg: DbgEng's output
  callbacks carry **engine** output, not the child's `printf`, and the target is spawned
  `CREATE_NO_WINDOW`. (`read_output` still returns engine/event text.)
- **`evaluate` ignores `frame_id`** — it evaluates in the engine's current frame.
- **Failing conditional breakpoints are silently skipped.** A conditional breakpoint whose
  condition fails to evaluate (out-of-scope symbol, typo) is silently **not** taken — C++ DbgEng
  parity; DbgEng provides no API notification for the evaluation failure.

## CI / sanitizer coverage notes

- **ThreadSanitizer** (the `tsan` Make target / CI job) covers the `dap-client` concurrency on
  **Linux** only (it's an LLVM/Linux facility); it does **not** cover the Windows engine-thread
  path on the MSVC toolchain. That interaction is instead covered by `#![forbid(unsafe_code)]` on
  `windbg-backend`, the `unsafe-gate` (unsafe confined to `dbgeng-sys`), the flag-only `AtomicBool`
  interrupt, and the live WinDbg pause tests.
- **Miri** runs over the neutral crates but is **excluded for `dbgeng-sys`**: COM FFI is not
  Miri-compatible.
- The **Windows CI lane** (`windows-latest`) is the only place the `cfg(windows)` code compiles:
  it runs `cargo build`/`clippy -D warnings`/`fmt --check`/the unsafe-gate/`cargo test` as hard
  gates, then builds the fixture (via `ilammy/msvc-dev-cmd` + `testdata/win/build.bat`) and runs
  the live `integration-windbg` suite (the live step self-skips if the fixture is absent).
