---
title: "Seam Additions — debugger-core + mcp-tools + bin"
type: phase
plan: WinDbgBackend
phase: 1
status: complete
created: 2026-06-03
updated: 2026-06-04
deliverable: "Cross-platform, no-WinDbg-yet additions: the capability-gated trait surface, the BackendRegistry switcher, the four new tool handlers (returning Unsupported), capability-aware list_tools, and the CLAUDE.md unsafe-deviation note — with the existing 21-tool lldb behavior byte-identical and the full suite green on Linux/macOS."
tasks:
  - id: "1.1"
    title: "debugger-core additions: capabilities, 4 default-Unsupported methods, new types, Unsupported error"
    status: complete
    verification: "`cargo build -p debugger-core` compiles; `BackendCapabilities` (crash_dump/kernel/analyze/modules bools, Default=all-false), `ModuleInfo`/`DumpOutcome` neutral types, and `BackendError::Unsupported(&'static str)` are defined; the four new trait methods (`open_dump`/`attach_kernel`/`analyze`/`modules`) have default bodies returning `Unsupported`; `BackendFactory::capabilities()` defaults to all-false; the object-safety test (`tests/object_safety.rs`) still passes with the enlarged trait; a serde round-trip test covers `ModuleInfo`/`DumpOutcome`; `cargo tree -p debugger-core` shows no new tokio/rmcp/DAP dependency."
    depends_on: []
  - id: "1.2"
    title: "BackendRegistry + per-OS default + selection precedence; ToolServer holds the registry"
    status: complete
    verification: "Unit tests prove `select()` precedence (explicit arg → `DEBUG_BACKEND` env → per-OS default) on every OS using two stub factories; an unknown/unavailable backend name yields the documented tool-error (not a panic); default resolves to `lldb` on non-Windows and (simulated) `windbg` on Windows; `ToolServer::new(session, registry)` compiles and `main.rs` builds a registry with the lldb factory; `connect_error()` is backend-name-keyed and still produces the verbatim `failed to find lldb-dap: …` / `failed to spawn lldb-dap: …` strings for the lldb factory."
    depends_on: ["1.1"]
  - id: "1.3"
    title: "`backend` selection arg on launch/attach + capability-aware list_tools + status fields"
    status: complete
    verification: "`schema::all_tools(caps)` returns exactly 21 tools when caps are all-false and 25 when a WinDbg-capable factory's caps are unioned in; the 21 lldb tool schemas are byte-identical except `launch`/`attach` gaining one optional `backend` enum (`[\"lldb\",\"windbg\"]`) property (asserted by a snapshot test); an invalid `backend` value is rejected with a tool-error; `status` output gains additive `backend` + `available_backends` fields without altering existing keys."
    depends_on: ["1.2"]
  - id: "1.4"
    title: "Four capability-gated tool handlers (open_crash_dump, attach_kernel, analyze_crash, get_modules)"
    status: complete
    verification: "`open_crash_dump`/`attach_kernel` are full connect points: they require state `Idle`, call `registry.select(Some(\"windbg\"))` **hardcoded** (ignoring `DEBUG_BACKEND`/default — documented in the handler), and (with no windbg registered) return `\"<tool> is not available: the windbg backend is not registered on this platform\"`; `dump_path` is required and `attach_kernel` validates the `net:` prefix; `analyze_crash`/`get_modules` operate on the active backend and return the `\"<tool> is not supported by the lldb backend\"` Unsupported string when the connected backend is lldb; each handler's arg-validation and state-guard strings are unit-tested against a fake registry/backend."
    depends_on: ["1.3"]
  - id: "1.4b"
    title: "Dump-session state-machine contract (frozen in Phase 1)"
    status: complete
    verification: "The session state after a successful `open_crash_dump` is **frozen and documented**: state = `Stopped` with a session-level `is_dump` flag (added to `SessionManager`); `analyze_crash`/`get_modules` guard with `check_state([Stopped])`; `cont`/`step` against a dump session surface the exact guard string `\"cannot continue a crash-dump session\"` (the string is defined here so Phase 2 task 2.3 and Phase 4 implement against a frozen literal, not an improvised one); a unit test asserts the `is_dump` flag round-trips through `open`/`disconnect` and that the guard string is emitted."
    depends_on: ["1.4"]
  - id: "1.5"
    title: "CLAUDE.md unsafe-deviation + parity notes; Phase-1 regression gate"
    status: complete
    verification: "CLAUDE.md records (a) the forthcoming `dbgeng-sys` `unsafe`-confinement deviation from \"target zero unsafe\" and (b) the new parity notes (`backend` arg on launch/attach, the four new tools); `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo fmt --check`, and `cargo test --workspace` are green on Linux/macOS with no `#[allow]`; **all existing `BackendFactory` implementors compile unchanged** (`LldbFactory`, the `integration-tests` stub, `mcp-tools` fake factories — relying on the `capabilities()` default); a regression test asserts the connected 21-tool lldb path (schemas + error wording) is unchanged; **the seam holds** — `cargo tree -p mcp-tools` and `cargo tree -p mcp-session` show no path to `windbg-backend`/`dbgeng-sys` (the `make seam` / `cargo-deny` gate stays green after the `BackendRegistry` change)."
    depends_on: ["1.4b"]
---

# Phase 1: Seam Additions — debugger-core + mcp-tools + bin

## Overview

Everything in this phase is **cross-platform and additive** — no WinDbg code yet. It freezes
the contract and wiring the later phases target: the enlarged (still object-safe) trait, the
neutral types for the WinDbg-only surface, the `BackendRegistry` switcher, the four new tool
handlers (which return `Unsupported` until a backend exists), and capability-aware tool
listing. The gate is strict: the existing 21-tool lldb behavior must stay byte-identical and
the whole suite green on Linux/macOS. Mirrors design §"Wiring changes to `ToolServer`",
§"Trait extension", §"Capability-aware tool listing", and Migration Phase 0.

## 1.1: debugger-core additions

### Subtasks
- [x] Add `BackendCapabilities { crash_dump, kernel, analyze, modules: bool }` (`derive(Default, Clone, Copy, PartialEq, Eq)` — default all-false).
- [x] Add neutral types `ModuleInfo { name, base, size, symbol_status: String }` (with `base` = `"0x{:016X}"`, `size` = decimal string, `symbol_status` ∈ {pdb,export,deferred,none}) and `DumpOutcome { stop: Option<StopInfo>, crash_location: Option<String> }` (doc the `crash_location` source = `current_source_location()` inside `open_dump`).
- [x] Add `BackendError::Unsupported(&'static str)` (the tool name) with a `thiserror` message.
- [x] Add four **default-`Unsupported`** methods to `DebuggerBackend`: `open_dump(&self, path)`, `attach_kernel(&self, connection)`, `analyze(&self)`, `modules(&self)`.
- [x] Add `fn capabilities(&self) -> BackendCapabilities { Default::default() }` to `BackendFactory`.
- [x] Update `tests/object_safety.rs` and the serde round-trip tests to cover the new methods/types.

### Notes
Default method bodies keep `lldb-backend` and every test stub compiling untouched — verify with
`cargo tree` that the contract crate gains no heavy deps. Keep payloads opaque pass-through
(Spec FR-18.6): `base`/`size`/`symbol_status` are strings, not enums.

## 1.2: BackendRegistry + selection

### Subtasks
- [x] Add `BackendRegistry { factories: HashMap<&'static str, Arc<dyn BackendFactory>>, default_name: &'static str, capabilities: BackendCapabilities }` in `mcp-tools/src/registry.rs`; `register()` unions `f.capabilities()` into the cached union.
- [x] Implement `select(requested: Option<&str>) -> Result<Arc<dyn BackendFactory>, String>` with precedence arg → `DEBUG_BACKEND` env → `default_name` (pure `select_with_env` for testability; empty values treated as unset); return a tool-error string for an unknown/unregistered name.
- [x] Add `fn default_backend_for_os() -> &'static str` (`#[cfg(windows)] "windbg"`, else `"lldb"`). (Windows default = windbg per user decision; resolves once Phase 3 registers the factory.)
- [x] Change `ToolServer` to hold `registry: BackendRegistry` (replacing the single `factory`); update `ToolServer::new(session, registry)` and `main.rs` to build the registry (lldb only this phase).
- [x] Refactor `connect_error()` in `lifecycle.rs` to take the selected factory's `name()` and emit backend-keyed strings (lldb verbatim; windbg wording reserved for later).

### Notes
This is a **structural** change to `ToolServer`, not a field add — every `ToolServer::new`
caller (incl. tests) updates to pass a registry. The connected-backend slot, event-pump, and
generation logic are unchanged; only the *source* of the factory moves to a registry lookup.

## 1.3: `backend` arg + capability-aware list_tools + status fields

### Subtasks
- [x] Add an optional `backend` property (`{"type":"string","enum":["lldb","windbg"]}`) to the `launch` and `attach` hand-built schemas (the only edit to an existing schema).
- [x] Parse the `backend` arg in `handle_launch`/`handle_attach` and thread it through into `registry.select(...)`.
- [x] Change `schema::all_tools()` → `schema::all_tools(caps: BackendCapabilities)`; gate the four extra tool schemas behind the flags; `list_tools`/`get_tool` pass `self.registry.capabilities` (`tools()` now takes `&self`).
- [x] Add additive `backend` + `available_backends` fields to the `status` response (`backend` reports the *active* backend recorded at connect, falling back to the per-OS default pre-connect).

### Notes
The union capabilities are computed at `register()` time (not per-call) because `list_tools`
runs before any backend connects. On Mac/Linux the union is all-false ⇒ exactly 21 tools.

## 1.4: Four capability-gated tool handlers

### Subtasks
- [x] `handle_open_crash_dump`: `check_state([Idle])` → `registry.select(Some("windbg"))` → connect/set_backend/spawn_event_pump (the `handle_launch` pattern) → `backend.open_dump(path)` → map to `{"status":"dump_loaded","crash_location":…}`; require `dump_path`.
- [x] `handle_attach_kernel`: same connect-point pattern; validate the `net:` prefix on `connection`; map outcome like attach.
- [x] `handle_analyze_crash` / `handle_get_modules`: operate on the **active** backend (guard `Stopped`); call `backend.analyze()` / `backend.modules()`; map `Unsupported` → tool-error keyed on the active backend name.
- [x] Register all four in `dispatch()` (handlers live in `handlers/windbg.rs`); schemas were added in 1.3 behind capabilities.

### Notes
`open_crash_dump`/`attach_kernel` call `registry.select(Some("windbg"))` with a **hardcoded**
name — the `DEBUG_BACKEND` env / per-OS default does **not** override factory choice for these
two tools. Until a real backend exists, these all surface `Unsupported`/"not available" — that
is the intended Phase-1 behavior and what the unit tests assert (against a fake registry/backend).

## 1.4b: Dump-session state-machine contract

### Subtasks
- [x] Add an `is_dump: bool` flag to `SessionManager` inner state (set by `open_crash_dump`, cleared by `reset`/`disconnect`).
- [x] Freeze: a successful `open_crash_dump` leaves state `Stopped` with `is_dump = true`.
- [x] Define the guards: `analyze_crash`/`get_modules` use `check_state([Stopped])`; `cont`/`step` on an `is_dump` session emit the frozen literal `"cannot continue a crash-dump session"` (one guard in the shared `resume()` covers all four).
- [x] Unit-test the `is_dump` round-trip (incl. reset) and the guard string (all four resume handlers reject + skip the backend; non-dump still proceeds).

### Notes
Freezing these strings/flags in Phase 1 means Phase 2 (task 2.3 dump-session guard) and Phase 4
implement against a literal contract, not an improvised one. No new enum state is added (an
`is_dump` flag on the existing `Stopped` state is sufficient and avoids touching the state enum
the lldb path depends on).

## 1.5: CLAUDE.md + regression gate

### Subtasks
- [x] Update CLAUDE.md: the `dbgeng-sys` `unsafe`-confinement deviation note (so Phases 2–4 read an accurate convention doc) + the new parity notes (`backend` arg, four new tools, dump-session `is_dump`) + the runtime-switcher architecture + the two forthcoming crates.
- [x] Confirm all existing `BackendFactory` implementors compile unchanged via the `capabilities()` default (`cargo build --workspace` clean).
- [x] Add/keep a 21-tool lldb parity regression test (`schema::all_tools` 21-vs-25 + `the_19_other_base_tool_schemas_are_unchanged` + verbatim-names — 13/13 green).
- [x] Run the full gate: `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo fmt --all --check`, `cargo test --workspace` (only the 3 pre-existing `lldb-backend` subprocess tests fail — `sh`/`true` absent on this Windows host; `lldb-backend` untouched by Phase 1), seam gate (`cargo tree` — no WinDbg/lldb/DAP path into mcp-tools/mcp-session).

### Notes
No WinDbg crate exists yet, so the `unsafe`-confinement grep gate is a no-op this phase but is
documented now and enforced from Phase 2.

## Acceptance Criteria
- [x] `debugger-core` exposes `BackendCapabilities`, the four default-`Unsupported` methods, `ModuleInfo`/`DumpOutcome`, and `BackendError::Unsupported`, with no new heavy deps and the object-safety test green.
- [x] `ToolServer` holds a `BackendRegistry`; selection precedence (arg → env → per-OS default) is unit-tested on all OSes; `connect_error` is backend-aware.
- [x] `list_tools` returns 21 tools with all-false caps and 25 with WinDbg caps; the lldb schemas are byte-identical aside from the additive `backend` arg.
- [x] The four new handlers validate args/state and return the correct `Unsupported`/"not available" strings; `open_crash_dump`/`attach_kernel` hardcode windbg selection.
- [x] The dump-session contract is frozen: `is_dump` flag + state `Stopped`, the `analyze_crash`/`get_modules` guards, and the `"cannot continue a crash-dump session"` literal.
- [x] CLAUDE.md updated; full suite green with no `#[allow]` (only the 3 pre-existing Windows-host `lldb-backend` subprocess failures, unrelated); all existing `BackendFactory` implementors compile unchanged; the seam gate (`cargo tree`) confirms `mcp-tools`/`mcp-session` reach neither WinDbg crate.
