#!/usr/bin/env bash
# Real-harness agent loop: codex replays a 3-step trajectory (exec_command echo, exec_command write, final text) over the
# Responses API. The second step is strict-gated on the first tool result containing the echoed string, so a pass means
# codex really ran the command and sent its output back.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${FAKELLM_BIN:-$ROOT/target/debug/fakellm}"
WORK=/tmp/fakellm-e2e-codex-trajectory
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

"$BIN" wire codex --endpoint "$URL" --format toml >"$WORK/home/config.toml"

curl -sf -X PUT "$URL/_fakellm/trajectories" -H 'content-type: application/json' -d '{
  "name": "echo-then-write",
  "on_divergence": "strict",
  "steps": [
    {"reply": {"tool_calls": [{"name": "exec_command", "arguments": {"cmd": "echo TRAJ_MARKER_7431"}}]}},
    {"expect": {"tool_results": [{"contains": "TRAJ_MARKER_7431"}]},
     "reply": {"tool_calls": [{"name": "exec_command", "arguments": {"cmd": "echo second > second.txt"}}]}},
    {"reply": {"text": "TRAJECTORY_DONE"}}
  ]
}' >/dev/null
curl -sf -X PUT "$URL/_fakellm/scripts" -d '[{"reply": {"trajectory": "echo-then-write"}}]' >/dev/null

set +e
CODEX_HOME="$WORK/home" FAKELLM_API_KEY=x timeout 120 \
  codex exec --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox -C "$WORK/cwd" "run the plan" \
  </dev/null >"$WORK/codex.out" 2>"$WORK/codex.err"
RC=$?
set -e

curl -s "$URL/_fakellm/requests?full=true" >"$WORK/requests.json" || true
echo "request log: $WORK/requests.json"
fail() { echo "FAIL: $1" >&2; cat "$WORK/codex.out" "$WORK/codex.err" >&2; "$BIN" ctl --endpoint "$URL" requests list >&2 || true; exit 1; }

[ "$RC" -eq 0 ] || fail "codex exit $RC"
grep -q TRAJECTORY_DONE "$WORK/codex.out" || fail "final text missing"
[ -f "$WORK/cwd/second.txt" ] || fail "second command did not run"
grep -q second "$WORK/cwd/second.txt" || fail "second.txt content"
grep -q TRAJ_MARKER_7431 "$WORK/requests.json" || fail "echo output never reached the server"
echo "codex trajectory e2e ok"
