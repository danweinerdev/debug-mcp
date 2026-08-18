#!/usr/bin/env bash
# plugins-sync.sh — generate (or check) the portable plugin trees.
#
# The repository's canonical agent plugin is the Claude Code plugin at plugin/
# (manifest plugin/.claude-plugin/plugin.json, hooks, and the debug-mcp-* skills).
# The OpenCode and Codex variants are GENERATED artifacts: two byte-identical,
# committed trees (.opencode-plugin/ and .codex-plugin/) holding a portable
# manifest, a portable README, and the same skills with host-neutral tool names.
# Never edit the generated trees by hand — edit plugin/ (or
# plugin/README.portable.md), then run `make plugins`.
#
#   scripts/plugins-sync.sh          # sync: regenerate both trees in place
#   scripts/plugins-sync.sh check    # check: fail if the committed trees drift
#
# Transforms applied to each skill on the way into the portable trees:
#   1. `mcp__debug__<tool>` -> `<tool>` (Claude Code's MCP tool naming is
#      host-specific; portable skills use the server's own tool names).
#   2. A tool-naming note is inserted after the H1 explaining host prefixes.
# Hooks are NOT ported: the nudge/session-reset hooks are Claude Code-specific;
# the portable variants are skills-only.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CANON="$ROOT/plugin"
OUT_DIRS=(".opencode-plugin" ".codex-plugin")
MODE="${1:-sync}"

[ -f "$CANON/.claude-plugin/plugin.json" ] || { echo "ERROR: canonical manifest $CANON/.claude-plugin/plugin.json not found" >&2; exit 1; }
[ -f "$CANON/README.portable.md" ] || { echo "ERROR: $CANON/README.portable.md not found" >&2; exit 1; }

VERSION="$(sed -n 's/^[[:space:]]*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$CANON/.claude-plugin/plugin.json" | head -1)"
[ -n "$VERSION" ] || { echo "ERROR: could not read version from canonical plugin.json" >&2; exit 1; }

# Render one portable tree into $1.
generate() {
  local dest="$1"
  mkdir -p "$dest/skills"

  # Portable manifest (Codex reads it; OpenCode discovers skills/ directly but
  # the manifest also marks the tree root). Version injected from the canonical
  # Claude manifest so a version bump moves all three manifests together.
  cat > "$dest/plugin.json" <<EOF
{
  "name": "debug-mcp",
  "version": "$VERSION",
  "description": "Interactive native debugging skills for the debug MCP server (lldb-dap on macOS/Linux, WinDbg/DbgEng on Windows): sessions, breakpoints, stepping, stack/variable inspection, expression evaluation, memory/disassembly, crash dumps, and raw debugger commands.",
  "author": { "name": "Daniel Weiner" },
  "repository": "https://github.com/danweinerdev/debug-mcp",
  "license": "MIT",
  "keywords": ["debug", "mcp", "lldb", "lldb-dap", "windbg", "dbgeng", "debugger", "native-debugging", "breakpoints"],
  "skills": "./skills/",
  "interface": {
    "displayName": "debug-mcp",
    "shortDescription": "Interactive native debugging via the debug MCP server",
    "longDescription": "Scenario skills that teach the model to debug native (C/C++/Rust/Go/Swift) programs live through the debug MCP server instead of print-debugging: launch/attach, breakpoints, stepping, inspection, expression evaluation, memory/disassembly, crash-dump analysis, and raw lldb/WinDbg commands.",
    "developerName": "Daniel Weiner",
    "category": "Developer Tools",
    "capabilities": ["Interactive"],
    "websiteURL": "https://github.com/danweinerdev/debug-mcp",
    "defaultPrompt": [
      "Debug why this binary segfaults using the debug MCP server.",
      "Attach to the running process and find where it hangs.",
      "Open this crash dump and tell me the faulting frame."
    ]
  }
}
EOF

  cp "$CANON/README.portable.md" "$dest/README.md"

  local sdir name
  for sdir in "$CANON"/skills/*/; do
    name="$(basename "$sdir")"
    [ -f "$sdir/SKILL.md" ] || continue
    mkdir -p "$dest/skills/$name"
    # 1) strip the Claude Code MCP prefix; 2) insert the naming note after the H1.
    sed 's/mcp__debug__//g' "$sdir/SKILL.md" | awk '
      { print }
      /^# / && !noted {
        print ""
        print "> Tool names below are the `debug` MCP server'"'"'s own tool names. Your host"
        print "> prefixes them with the server name (e.g. `debug_launch` in OpenCode) —"
        print "> register the server under the name `debug` so they resolve."
        noted = 1
      }
    ' > "$dest/skills/$name/SKILL.md"
  done
}

case "$MODE" in
  sync)
    for d in "${OUT_DIRS[@]}"; do
      rm -rf "${ROOT:?}/$d"
      generate "$ROOT/$d"
      echo "generated $d/ (v$VERSION)"
    done
    ;;
  check)
    TMP="$(mktemp -d)"
    trap 'rm -rf "$TMP"' EXIT
    generate "$TMP/expected"
    status=0
    for d in "${OUT_DIRS[@]}"; do
      if ! diff -r "$TMP/expected" "$ROOT/$d" >/dev/null 2>&1; then
        echo "DRIFT: $d/ does not match a fresh generation — run 'make plugins' and commit" >&2
        diff -r "$TMP/expected" "$ROOT/$d" >&2 || true
        status=1
      fi
    done
    [ "$status" -eq 0 ] && echo "plugins-check ok"
    exit "$status"
    ;;
  *)
    echo "usage: $0 [sync|check]" >&2
    exit 2
    ;;
esac
