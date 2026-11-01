#!/usr/bin/env bash
# Real-harness smoke test: the pi coding agent CLI (from the pi-mono clone) against fakellm, configured by `fakellm wire pi`.
# pi is not installed globally; it runs from source with node's TypeScript stripping and pi's source resolver. Set PI_MONO
# to the clone (default: $RESEARCH_DIR or ~/Developer/fakellm-research, then /pi-mono). Skips (exit 0) if the clone or its
# node_modules is missing. Everything lives in /tmp/fakellm-e2e-pi; the user's ~/.pi is never touched.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${FAKELLM_BIN:-$ROOT/target/debug/fakellm}
PI_MONO=${PI_MONO:-${RESEARCH_DIR:-$HOME/Developer/fakellm-research}/pi-mono}
if [ ! -f "$PI_MONO/packages/coding-agent/src/cli.ts" ] || [ ! -d "$PI_MONO/node_modules" ]; then
  echo "skip: pi-mono clone with node_modules not found at $PI_MONO (set PI_MONO)"
  exit 0
fi

T=/tmp/fakellm-e2e-pi
rm -rf "$T"
mkdir -p "$T/agent" "$T/cwd" "$T/home"

"$BIN" --port 0 >"$T/server.out" 2>"$T/server.err" &
PID=$!
trap 'kill $PID 2>/dev/null || true' EXIT
URL=""
for _ in $(seq 100); do
  URL=$(grep -Eo 'http://[0-9.]+:[0-9]+' "$T/server.out" | head -1 || true)
  [ -n "$URL" ] && break
  sleep 0.05
done
[ -n "$URL" ] || { echo "fakellm did not start" >&2; exit 1; }
fail() { echo "FAIL: $1" >&2; exit 1; }

"$BIN" wire pi --endpoint "$URL" --file models.json >"$T/agent/models.json"
curl -sf -X PUT "$URL/_fakellm/scripts" -d '[
 {"name":"done","match":{"has_tool_result":true},"reply":{"text":"PI_LOOP_DONE"}},
 {"name":"hi","match":{"last_user":"say hi"},"reply":{"text":"FAKELLM_OK"}},
 {"name":"bash","match":{"has_tool":"bash"},"reply":{"tool_calls":[{"name":"bash","arguments":{"command":"echo PI_MARKER > made.txt"}}]}}
]' >/dev/null

RESOLVER=$(node -p 'require("node:url").pathToFileURL(process.argv[1]).href' "$PI_MONO/packages/coding-agent/src/experimental/source-resolver.ts")
run_pi() {
  (cd "$T/cwd" && HOME="$T/home" PI_CODING_AGENT_DIR="$T/agent" timeout 120 \
    node --import "$RESOLVER" "$PI_MONO/packages/coding-agent/src/cli.ts" -p --model fakellm/fake-model "$1" </dev/null >"$T/$2.out" 2>"$T/$2.err") \
    || { echo "pi failed ($2)" >&2; cat "$T/$2.err" >&2; exit 1; }
}

# Scenario 1: one prompt, one text answer.
run_pi "say hi" s1
grep -q FAKELLM_OK "$T/s1.out" || { cat "$T/s1.out" "$T/s1.err" >&2; fail "s1: FAKELLM_OK missing"; }
curl -sf "$URL/_fakellm/requests" | grep -q '"path":"/v1/chat/completions"' || fail "s1: no chat completion logged"

# Scenario 2: bash tool loop: pi runs the command, sends the result back, and prints the final text.
run_pi "make a file" s2
grep -q PI_LOOP_DONE "$T/s2.out" || { cat "$T/s2.out" "$T/s2.err" >&2; fail "s2: PI_LOOP_DONE missing"; }
grep -q PI_MARKER "$T/cwd/made.txt" || fail "s2: bash did not run"
echo "pi e2e ok"
