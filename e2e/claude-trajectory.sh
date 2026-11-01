#!/usr/bin/env bash
# Claude Code agent loop: a trajectory written for a generic "shell" tool {cmd} is mapped by tool_map onto Claude Code's
# Bash {command}. Passing means Claude Code accepted the renamed call, ran it, and returned the result to the next step.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
BIN=${FAKELLM_BIN:-$root/target/debug/fakellm}
work=/tmp/fakellm-e2e-claude-trajectory
rm -rf "$work"; mkdir -p "$work/config" "$work/cwd"
"$BIN" --port 0 >"$work/server.out" 2>"$work/server.err" &
pid=$!
trap 'kill $pid 2>/dev/null || true' EXIT
url=""
for _ in $(seq 50); do url=$(grep -Eo 'http://[0-9.]+:[0-9]+' "$work/server.out" | head -1 || true); [ -n "$url" ] && break; sleep 0.1; done
[ -n "$url" ] || { echo "fakellm did not start" >&2; exit 1; }

curl -sf -X PUT "$url/_fakellm/trajectories" -H 'content-type: application/json' -d '{
  "name": "shell-then-answer",
  "on_divergence": "strict",
  "tool_map": {"shell": {"name": "Bash", "args": {"command": "$cmd"}}},
  "steps": [
    {"reply": {"tool_calls": [{"name": "shell", "arguments": {"cmd": "echo CLAUDE_MARKER_5520 > made.txt && cat made.txt"}}]}},
    {"expect": {"tool_results": [{"contains": "CLAUDE_MARKER_5520"}]}, "reply": {"text": "CLAUDE_TRAJ_DONE"}}
  ]
}' >/dev/null
# Side requests (titles, topic detection) carry no tools; only the agent loop requests do.
curl -sf -X PUT "$url/_fakellm/scripts" -H 'content-type: application/json' -d '[
  {"name": "side", "match": {"no_tools": true}, "reply": {"text": "ok"}},
  {"name": "agent", "reply": {"trajectory": "shell-then-answer"}}
]' >/dev/null

cd "$work/cwd"
set +e
env CLAUDE_CONFIG_DIR="$work/config" ANTHROPIC_BASE_URL="$url" ANTHROPIC_API_KEY=fakellm \
  DISABLE_TELEMETRY=1 DISABLE_AUTOUPDATER=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
  timeout 120 claude -p "run the plan" --output-format json --no-session-persistence \
  --permission-mode bypassPermissions >"$work/result.json" 2>"$work/claude.err"
rc=$?
set -e
curl -s "$url/_fakellm/requests?full=true" >"$work/requests.json" || true
echo "request log: $work/requests.json"
fail() { echo "FAIL: $1" >&2; cat "$work/result.json" "$work/claude.err" >&2; exit 1; }

[ "$rc" -eq 0 ] || fail "claude exit $rc"
grep -q CLAUDE_TRAJ_DONE "$work/result.json" || fail "final text missing"
grep -q CLAUDE_MARKER_5520 "$work/cwd/made.txt" || fail "Bash did not run"
grep -q '"Bash"' "$work/requests.json" || fail "no Bash tool in the request log"
echo "claude trajectory e2e ok"
