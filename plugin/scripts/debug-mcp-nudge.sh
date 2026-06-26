#!/usr/bin/env bash
# debug-mcp-nudge.sh — PreToolUse hook for Bash.
#
# Goal: when the model reaches for a way to investigate a crash or a wrong value
# that the interactive debugger would answer better — driving lldb/gdb by hand,
# or re-running a native binary to watch it fail — inject a one-time,
# NON-BLOCKING nudge toward the debug MCP tools (mcp__debug__launch,
# mcp__debug__set_breakpoint, …). We never block: running a program, a
# build, or a one-off lldb command are all legitimately the right tool much of
# the time. The nudge is advisory context only.
#
# Contract (Claude Code PreToolUse hook):
#   - stdin: a JSON object with at least { tool_name, tool_input, session_id }.
#     Field spellings vary across versions, so we read defensively.
#   - stdout (exit 0): a JSON object whose
#       .hookSpecificOutput.additionalContext
#     is injected into the model's context for its next turn. We emit NO
#     permission decision, so the tool runs exactly as it would have.
#   - Any failure path exits 0 with no stdout (fail-open): a broken hook must
#     never wedge a command.
#
# Throttle: at most one nudge per (session, kind) per NUDGE_COOLDOWN_SECONDS,
# tracked by a stamp file under the session state dir, so a debugging-heavy turn
# doesn't get spammed.

set -euo pipefail

# Fail-open helper: emit nothing, let the tool proceed untouched.
pass_through() { exit 0; }

# jq is required to parse the event and build valid JSON output. If it's
# missing, silently pass through rather than risk malformed stdout.
command -v jq >/dev/null 2>&1 || pass_through

input="$(cat || true)"
[[ -n "${input}" ]] || pass_through

# Tolerate field-name drift: tool_name|tool, and the session id under a few keys.
tool="$(printf '%s' "${input}" | jq -r '.tool_name // .tool // empty' 2>/dev/null || true)"
session="$(printf '%s' "${input}" | jq -r '.session_id // .sessionId // .session // "default"' 2>/dev/null || true)"
[[ "${tool}" == "Bash" ]] || pass_through

cmd="$(printf '%s' "${input}" | jq -r '.tool_input.command // empty' 2>/dev/null || true)"
[[ -n "${cmd}" ]] || pass_through

# Classify the command into the kind of debugging nudge (if any) it warrants.
# "kind" doubles as the throttle key so a direct-lldb nudge and a run-binary
# nudge are rate-limited independently.
#   debugger  - driving lldb / lldb-dap / gdb directly by hand.
#   runbin    - launching a native executable directly (./a.out, a debug-built
#               target, or `cargo run`) — exactly the thing launch+breakpoints
#               replace when you're chasing a crash or wrong value.
# Anything else (builds, tests, greps, file ops, the debug-mcp test suite
# itself) is left alone.
kind=""

# --- Direct debugger invocation. -------------------------------------------
# Match lldb / lldb-dap / lldb-vscode / gdb as a *command word* (start of line,
# after a pipe/;/&&/||, or after `sudo`/`env`), not as a substring of a path or
# flag. `lldb --version` and friends are version probes, not debugging — skip.
if printf '%s' "${cmd}" | grep -Eq '(^|[|;&]|sudo |env )[[:space:]]*(lldb-dap|lldb-vscode|lldb|gdb)([[:space:]]|$)'; then
  if ! printf '%s' "${cmd}" | grep -Eq -- '--version|-v[[:space:]]|--help'; then
    kind="debugger"
  fi
fi

# --- Running a native binary directly. -------------------------------------
# Heuristic: a path-qualified executable invocation (./foo, ../foo, /abs/foo,
# bin/foo, target/debug/foo) or a `cargo run`. These are the moments where, if
# the goal is "why does this crash / why is this value wrong", launching it
# under the debugger with a breakpoint beats reading stderr after the fact.
# Deliberately conservative: bare PATH commands (make, ls, grep, …) don't match,
# and the debug-mcp project's own `cargo test` / `make` flows are untouched.
if [[ -z "${kind}" ]]; then
  # A command word (start of line, or after a pipe/;/&&/||, optionally behind
  # sudo/env) that is either:
  #   (a) an explicitly path-qualified executable: ./foo, ../foo, /abs/foo, or
  #   (b) a path that descends through a known build-output dir: any prefix then
  #       bin/ | build/ | target/debug/ | target/release/ followed by a name,
  #       which also matches a bare leading `target/release/foo` or `bin/foo`.
  cw='(^|[|;&]|sudo |env )[[:space:]]*'
  if printf '%s' "${cmd}" | grep -Eq "${cw}(\./|\.\./|/)[A-Za-z0-9_.-]+" \
     || printf '%s' "${cmd}" | grep -Eq "${cw}[A-Za-z0-9_./-]*(bin|build|target/debug|target/release)/[A-Za-z0-9_.-]+"; then
    kind="runbin"
  elif printf '%s' "${cmd}" | grep -Eq "${cw}cargo[[:space:]]+run([[:space:]]|$)"; then
    kind="runbin"
  fi
fi

[[ -n "${kind}" ]] || pass_through

# --- Throttle: one nudge per (session, kind) per cooldown window. -----------
NUDGE_COOLDOWN_SECONDS="${DEBUG_MCP_NUDGE_COOLDOWN:-900}"
state_root="${CLAUDE_PROJECT_DIR:-${TMPDIR:-/tmp}}"
state_dir="${state_root}/.debug-mcp-plugin/nudge"
mkdir -p "${state_dir}" 2>/dev/null || state_dir="${TMPDIR:-/tmp}"
stamp="${state_dir}/${session//[^A-Za-z0-9_-]/_}.${kind}"

now="$(date +%s 2>/dev/null || echo 0)"
if [[ -f "${stamp}" ]]; then
  last="$(cat "${stamp}" 2>/dev/null || echo 0)"
  [[ "${last}" =~ ^[0-9]+$ ]] || last=0
  if [[ "${now}" -gt 0 && $((now - last)) -lt "${NUDGE_COOLDOWN_SECONDS}" ]]; then
    pass_through
  fi
fi
printf '%s' "${now}" > "${stamp}" 2>/dev/null || true

# --- The nudge. Tailored per kind. -----------------------------------------
if [[ "${kind}" == "debugger" ]]; then
  message="The debug MCP server is available and wraps lldb-dap as live, stateful debug tools — usually better than driving lldb/gdb by hand through Bash, where each invocation is a fresh non-interactive process that can't hold breakpoints or a stopped state across calls:
- Start/attach a session → mcp__debug__launch (program, args, cwd, env, stop_on_entry) or mcp__debug__attach (pid / wait_for).
- Breakpoints → mcp__debug__set_breakpoint (file+line, optional condition), mcp__debug__set_function_breakpoint, mcp__debug__list_breakpoints, mcp__debug__remove_breakpoint.
- Drive execution → mcp__debug__continue, mcp__debug__step_over / step_into / step_out, mcp__debug__pause, mcp__debug__status.
- Inspect at a stop → mcp__debug__backtrace, mcp__debug__threads, mcp__debug__variables, mcp__debug__evaluate, mcp__debug__read_memory, mcp__debug__disassemble.
- Still need a raw LLDB command? mcp__debug__run_command runs it inside the *same live session* (state preserved), unlike a one-shot lldb in Bash.
Driving lldb/gdb directly is still fine for a quick scripted one-liner — proceed if that's what you want. (See the debug-mcp-debugging and debug-mcp-breakpoints skills.)"
else
  message="The debug MCP server is available and can run this program under lldb-dap with breakpoints and live inspection — usually far more informative than running it bare and reading stderr after it crashes or prints the wrong value:
- Launch it under the debugger → mcp__debug__launch (program, args, cwd, env, stop_on_entry:true to stop before main).
- Set a breakpoint where the bug is suspected → mcp__debug__set_breakpoint (file+line, optional condition like i==42) or mcp__debug__set_function_breakpoint.
- Run to it and inspect → mcp__debug__continue, then mcp__debug__backtrace / mcp__debug__variables / mcp__debug__evaluate at the stop; step with mcp__debug__step_over / step_into / step_out.
- If it segfaults, the stop lands on the fault — read mcp__debug__backtrace and the offending mcp__debug__variables / mcp__debug__read_memory instead of guessing from a core-less crash.
- Captured stdout/stderr is still available via mcp__debug__read_output.
Running the program directly is still right when you just need its output or exit code — proceed if that's the goal. (See the debug-mcp-debugging, debug-mcp-breakpoints, and debug-mcp-inspection skills.)"
fi

jq -n --arg ctx "${message}" '{
  hookSpecificOutput: {
    hookEventName: "PreToolUse",
    additionalContext: $ctx
  }
}'
exit 0
