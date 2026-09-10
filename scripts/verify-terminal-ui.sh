#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARTIFACT_DIR="${BUT_DESIGN_LOOP_DIR:-/tmp/but-design-loop}"

cd "$ROOT"
mkdir -p "$ARTIFACT_DIR"

CURRENT_NOFILE="$(ulimit -n || true)"
if [[ "$CURRENT_NOFILE" != "unlimited" && "$CURRENT_NOFILE" -lt 4096 ]]; then
  ulimit -n 4096 2>/dev/null || true
fi

echo "== cargo fmt =="
cargo fmt --check

echo "== rust tests =="
cargo test -- --test-threads=1

echo "== python tests =="
uv run --with pytest python -m pytest -q

dump_screen() {
  local name="$1"
  shift
  local state_dir="/tmp/but-rust-${name}"
  rm -rf "$state_dir"
  echo "== dump ${name} =="
  cargo run -q -p browser-use-tui -- --state-dir "$state_dir" "$@" --dump-screen >"$ARTIFACT_DIR/${name}.txt"
}

dump_screen empty
dump_screen setup --overlay setup
dump_screen account --overlay account
dump_screen model --overlay model
dump_screen done --seed-demo done --select-latest
dump_screen running --seed-demo running --select-latest
dump_screen cancelled --seed-demo cancelled --select-latest
dump_screen browser --seed-demo done --select-latest --overlay browser
dump_screen history --seed-demo done --select-latest --overlay history
dump_screen developer --seed-demo done --select-latest --overlay developer

echo "== real terminal smoke =="
scripts/tui-terminal-smoke.py

echo "terminal UI verification passed"
echo "artifacts: ${ARTIFACT_DIR}"
