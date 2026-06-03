# Workspace lint/test gates for the Rust port.
#
# Run from anywhere; all targets pin the workspace manifest so no `cd` is needed.
MANIFEST := $(CURDIR)/Cargo.toml
CARGO := cargo

.PHONY: all build clippy fmt fmt-check test integration tsan check seam

# The full gate, in the order the phase verification runs it.
all: fmt-check build clippy test seam

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

# ThreadSanitizer over the dap-client concurrency tests (stop waiter, read-loop EOF
# recovery, send/correlate/cancel). Needs nightly + rust-src; builds std instrumented.
tsan:
	RUSTFLAGS="-Zsanitizer=thread" RUSTDOCFLAGS="-Zsanitizer=thread" \
		$(CARGO) +nightly test --manifest-path $(MANIFEST) -p dap-client \
		-Zbuild-std --target x86_64-unknown-linux-gnu \
		--test client --test read_loop --test stop_waiter

check: build clippy fmt-check test

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
