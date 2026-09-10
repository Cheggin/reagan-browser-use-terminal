#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="${OUT_DIR:-"$ROOT/target/dev-bin"}"
AGENT_TOOLS_DIR="$ROOT/target/debug/agent-tools"

cd "$ROOT"
cargo build -p browser-use-tui -p browser-use-cli

mkdir -p "$OUT_DIR"

write_agent_tools() {
  "$ROOT/scripts/install-agent-ripgrep.sh" "$AGENT_TOOLS_DIR"
}

write_tui_wrapper() {
  local name="$1"
  local path="$OUT_DIR/$name"
  cat >"$path" <<EOF
#!/bin/sh
export BUT_AGENT_TOOLS_DIR="$AGENT_TOOLS_DIR"
export PYTHONPATH="$ROOT/python\${PYTHONPATH:+:\$PYTHONPATH}"
exec "$ROOT/target/debug/but" "\$@"
EOF
  chmod 0755 "$path"
}

write_hybrid_wrapper() {
  local name="$1"
  local path="$OUT_DIR/$name"
  cat >"$path" <<EOF
#!/bin/sh
export BUT_AGENT_TOOLS_DIR="$AGENT_TOOLS_DIR"
export PYTHONPATH="$ROOT/python\${PYTHONPATH:+:\$PYTHONPATH}"
if [ "\$#" -eq 0 ]; then
  exec "$ROOT/target/debug/but"
fi
exec "$ROOT/target/debug/browser-use-terminal" "\$@"
EOF
  chmod 0755 "$path"
}

write_agent_tools
write_hybrid_wrapper browser
write_hybrid_wrapper browser-use
write_hybrid_wrapper browser-use-terminal
write_tui_wrapper but

printf 'Local dev commands written to %s\n' "$OUT_DIR"
printf 'Run: export PATH="%s:$PATH"\n' "$OUT_DIR"
