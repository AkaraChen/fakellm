#!/usr/bin/env bash
# Real-harness smoke test: run the codex CLI against fakellm (Responses API over HTTP SSE).
# Uses an isolated CODEX_HOME so ~/.codex is never touched.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${FAKELLM_BIN:-$ROOT/target/debug/fakellm}"
WORK=/tmp/fakellm-e2e-codex
rm -rf "$WORK"
mkdir -p "$WORK/home" "$WORK/cwd"

"$BIN" --port 0 >"$WORK/server.out" 2>"$WORK/server.err" &
SERVER=$!
trap 'kill $SERVER 2>/dev/null || true' EXIT

URL=""
for _ in $(seq 100); do
  URL="$(head -n1 "$WORK/server.out" | grep -o 'http://[0-9.]*:[0-9]*' || true)"
  [ -n "$URL" ] && break
  sleep 0.05
done
[ -n "$URL" ] || { echo "fakellm did not start" >&2; exit 1; }
PORT="${URL##*:}"

"$BIN" wire codex --endpoint "$URL" --format toml >"$WORK/home/config.toml"
grep -q "127.0.0.1:$PORT" "$WORK/home/config.toml"

run_codex() {
  CODEX_HOME="$WORK/home" FAKELLM_API_KEY=x timeout 120 \
    codex exec --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox \
    -C "$WORK/cwd" "$1" </dev/null >"$WORK/$2.out" 2>"$WORK/$2.err" || { echo "codex failed ($2)" >&2; cat "$WORK/$2.err" >&2; exit 1; }
}

# Scenario 1: plain text answer.
curl -sf -X PUT "$URL/_fakellm/scripts" -d '[{"reply":{"text":"FAKELLM_OK"}}]' >/dev/null
run_codex "say hi" s1
grep -q FAKELLM_OK "$WORK/s1.out" || { echo "s1: FAKELLM_OK missing" >&2; cat "$WORK/s1.out" >&2; exit 1; }

curl -sf "$URL/_fakellm/requests?full=true" >"$WORK/s1.requests.json"
grep -q '/v1/responses' "$WORK/s1.requests.json" || { echo "s1: no POST /v1/responses logged" >&2; exit 1; }
grep -q '"developer"' "$WORK/s1.requests.json" || { echo "s1: no developer message logged" >&2; exit 1; }

# Scenario 2: tool call, then text once the tool result is back.
curl -sf -X DELETE "$URL/_fakellm/requests" >/dev/null || true
curl -sf -X PUT "$URL/_fakellm/scripts" -d '[
 {"match":{"has_tool_result":true},"reply":{"text":"TOOL_DONE"}},
 {"reply":{"tool_calls":[{"name":"exec_command","arguments":{"cmd":"echo hello > made.txt"}}]}}
]' >/dev/null
run_codex "make a file" s2
grep -q TOOL_DONE "$WORK/s2.out" || { echo "s2: TOOL_DONE missing" >&2; cat "$WORK/s2.out" >&2; exit 1; }
[ -f "$WORK/cwd/made.txt" ] || { echo "s2: tool did not run" >&2; exit 1; }
curl -sf "$URL/_fakellm/requests?full=true" >"$WORK/s2.requests.json"
echo "codex e2e ok"
