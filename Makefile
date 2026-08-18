# Workspace lint/test gates for the Rust port.
#
# Run from anywhere; all targets pin the workspace manifest so no `cd` is needed.
MANIFEST := $(CURDIR)/Cargo.toml
CARGO := cargo

.PHONY: all build clippy fmt fmt-check test integration integration-windbg tsan check seam unsafe-gate audit deny ready plugins plugins-check

# The full gate, in the order the phase verification runs it.
all: fmt-check build clippy test seam unsafe-gate plugins-check

build:
	$(CARGO) build --manifest-path $(MANIFEST) --workspace

# No `#[allow]` suppressions — warnings are fixed at the source. `--all-features` so the
# `integration` test code (behind the integration-tests crate's feature) is linted too.
clippy:
	$(CARGO) clippy --manifest-path $(MANIFEST) --workspace --all-targets --all-features -- -D warnings

fmt:
	$(CARGO) fmt --manifest-path $(MANIFEST) --all

fmt-check:
	$(CARGO) fmt --manifest-path $(MANIFEST) --all -- --check

test:
	$(CARGO) test --manifest-path $(MANIFEST) --workspace

# Live integration + differential-parity suite (Phase 6). Requires lldb-dap and the
# compiled C fixtures; run `make -C testdata` first. Each test skips cleanly (logs +
# passes) when lldb-dap/fixtures are absent. Single-threaded: the suites share the
# lldb-dap binary and the crash scenarios kill subprocesses by pid. The differential
# Rust-vs-Go lane skips unless a Go `lldb-debug-mcp` binary is on PATH (or GO_DEBUG_MCP_BIN).
integration:
	$(CARGO) build --manifest-path $(MANIFEST) -p debug-mcp
	$(CARGO) test --manifest-path $(MANIFEST) -p mcp-tools --features integration -- --test-threads=1

# Live WinDbg integration suite (Windows-only), parallel to `integration` above. Requires
# the compiled fixture `testdata/win/test_target.exe` (build it via `testdata/win/build.bat`
# from a VS x64 Native Tools prompt). Each test skips cleanly (logs + passes) when the
# fixture is absent. Single-threaded because DbgEng keeps process-global engine state.
# Core debugging is driven by the OS-bundled DbgEng (System32); tests that need the full
# Debugging Tools for Windows (the `!analyze` rich report) degrade gracefully when only the
# OS-bundled DbgEng is present.
integration-windbg:
	$(CARGO) build --manifest-path $(MANIFEST) -p debug-mcp
	$(CARGO) test --manifest-path $(MANIFEST) -p mcp-tools --features integration-windbg -- --test-threads=1

# ThreadSanitizer over the dap-client concurrency tests (stop waiter, read-loop EOF
# recovery, send/correlate/cancel). Needs nightly + rust-src; builds std instrumented.
tsan:
	RUSTFLAGS="-Zsanitizer=thread" RUSTDOCFLAGS="-Zsanitizer=thread" \
		$(CARGO) +nightly test --manifest-path $(MANIFEST) -p dap-client \
		-Zbuild-std --target x86_64-unknown-linux-gnu \
		--test client --test read_loop --test stop_waiter

check: build clippy fmt-check test unsafe-gate

# Seam guarantee (Spec FR-18): debugger-core must carry no tokio/rmcp/DAP edge, and
# the neutral crates must not name a DAP/lldb crate.
seam:
	@! $(CARGO) tree --manifest-path $(MANIFEST) -p debugger-core | grep -E '\b(tokio|rmcp|dap-client|lldb-backend)\b' \
		|| (echo "SEAM VIOLATION: debugger-core pulled in a runtime/DAP crate" && exit 1)
	@! $(CARGO) tree --manifest-path $(MANIFEST) -p mcp-tools --edges normal | grep -E '\b(dap-client|lldb-backend)\b' \
		|| (echo "SEAM VIOLATION: mcp-tools depends on a backend crate" && exit 1)
	@! $(CARGO) tree --manifest-path $(MANIFEST) -p mcp-session --edges normal | grep -E '\b(dap-client|lldb-backend)\b' \
		|| (echo "SEAM VIOLATION: mcp-session depends on a backend crate" && exit 1)
	@echo "seam ok"

# Confined-unsafe guarantee (design Decision 1): the `unsafe` keyword may appear in
# `crates/**/src/*.rs` ONLY under `crates/dbgeng-sys/`. Scans for the `unsafe` keyword in
# real code (`unsafe {`, `unsafe fn`, `unsafe impl`, `unsafe trait`), excluding dbgeng-sys
# and excluding line/doc comments that merely mention the word. Same shell style as `seam`.
unsafe-gate:
	@! grep -rnE '(^|[^[:alnum:]_])unsafe[[:space:]]*(\{|fn|impl|trait)' \
		--include='*.rs' crates \
		| grep -v 'crates/dbgeng-sys/' \
		| grep -vE '^[^:]+:[0-9]+:[[:space:]]*(//|//!|/\*|\*)' \
		|| (echo "UNSAFE-GATE VIOLATION: 'unsafe' found outside crates/dbgeng-sys/" && exit 1)
	@echo "unsafe-gate ok"

# Security-advisory / CVE scan of the locked dependency tree against the RustSec advisory
# database (cargo-audit). The authoritative vulnerability scanner for the `ready` gate.
# Install once with `cargo install cargo-audit`.
audit:
	$(CARGO) audit --file $(CURDIR)/Cargo.lock

# Dependency-policy gate (cargo-deny): the license allow-list (deny.toml), banned/duplicate
# crates, and source registries. Security advisories are owned by the `audit` target above
# (cargo-audit is the authoritative scanner and needs no git/network reachability that
# cargo-deny's advisory-DB fetch requires), so this runs the offline policy checks only — they
# enforce reliably in any environment. Run `make audit deny` (or `make ready`) for full
# supply-chain coverage. Install once with `cargo install cargo-deny`.
deny:
	$(CARGO) deny --manifest-path $(MANIFEST) check bans licenses sources

# Pre-ship readiness gate: the full test suite plus the supply-chain gates (audit + deny).
# Run before cutting a release or shipping a binary. (For the lint/format/seam/unsafe gates,
# run `make all`; `make ready` focuses on tests + dependency health.)
ready: test audit deny

# --- Portable (OpenCode/Codex) plugin trees ------------------------------------
#
# .opencode-plugin/ and .codex-plugin/ are generated from the canonical Claude
# Code plugin at plugin/ by scripts/plugins-sync.sh — the same skills, written
# once per harness install convention. They are committed (they are what the
# other harnesses install) and drift-gated: `make all` runs plugins-check, which
# fails when either tree does not match a fresh generation. Never edit them by
# hand — edit plugin/ (or plugin/README.portable.md), then `make plugins`.
plugins:
	@bash scripts/plugins-sync.sh

plugins-check:
	@bash scripts/plugins-sync.sh check
