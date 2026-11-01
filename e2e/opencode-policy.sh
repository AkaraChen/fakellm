#!/usr/bin/env bash
# opencode agent loop driven by a policy script on the default runtime (txiki.js embedded in the fakellm binary): call bash
# once, then answer with the tool output it got back. Needs opencode only, no node. Everything lives under /tmp/fakellm-e2e-opencode-policy.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${FAKELLM_BIN:-$ROOT/target/debug/fakellm}
T=/tmp/fakellm-e2e-opencode-policy
rm -rf "$T"
mkdir -p "$T"/{project,home,config,data,cache,state}

"$BIN" --port 0 >"$T/server.out" 2>"$T/server.err" &
PID=$!
trap 'kill $PID 2>/dev/null || true' EXIT

for _ in $(seq 100); do
  PORT=$(grep -oE '127\.0\.0\.1:[0-9]+' "$T/server.out" | head -1 | cut -d: -f2 || true)
  [ -n "${PORT:-}" ] && break
  sleep 0.1
done
[ -n "${PORT:-}" ] || { echo "fakellm did not start" >&2; cat "$T/server.err" >&2; exit 1; }
URL=http://127.0.0.1:$PORT

cat >"$T/policy.json" <<'JSON'
{"name": "once", "source": "export default function decide(request, context) {\n  const last = request.messages.at(-1);\n  if (last.role !== 'tool') {\n    return { tool_calls: [{ name: 'bash', arguments: { command: 'echo POLICY_MARKER_9917', description: 'echo marker' } }], state: { called: true } };\n  }\n  const out = last.content.map((p) => p.text ?? '').join('');\n  return { text: 'POLICY_DONE saw=' + out.includes('POLICY_MARKER_9917'), state: { called: true } };\n}\n"}
JSON
curl -sf -X PUT "$URL/_fakellm/policies" -H 'content-type: application/json' --data @"$T/policy.json" >/dev/null
curl -sf -X PUT "$URL/_fakellm/scripts" -H 'content-type: application/json' -d '[
  {"name": "title", "match": {"no_tools": true}, "reply": {"text": "Greeting"}},
  {"name": "agent", "reply": {"policy": "once"}}
]' >/dev/null

cat >"$T/project/opencode.json" <<JSON
{
  "\$schema": "https://opencode.ai/config.json",
  "model": "fakellm/fake-model",
  "permission": {"bash": "allow", "edit": "allow"},
  "provider": {"fakellm": {
    "npm": "@ai-sdk/openai-compatible", "name": "fakellm",
    "options": {"baseURL": "$URL/v1", "apiKey": "fakellm"},
    "models": {"fake-model": {"name": "fake-model", "tool_call": true}}
  }}
}
JSON

set +e
(cd "$T/project" && HOME="$T/home" XDG_CONFIG_HOME="$T/config" XDG_DATA_HOME="$T/data" XDG_CACHE_HOME="$T/cache" XDG_STATE_HOME="$T/state" \
  OPENCODE_CONFIG="$T/project/opencode.json" OPENCODE_DISABLE_MODELS_FETCH=1 OPENCODE_DISABLE_AUTOUPDATE=1 \
  timeout 120 opencode run --model fakellm/fake-model "run the plan" </dev/null >"$T/opencode.out" 2>"$T/opencode.err")
RC=$?
set -e

curl -s "$URL/_fakellm/requests?full=true" >"$T/requests.json" || true
curl -s "$URL/_fakellm/policies/once" >"$T/policy-info.json" || true
echo "== opencode exit $RC"; cat "$T/opencode.out"
echo "== request log: $T/requests.json"
"$BIN" ctl --endpoint "$URL" requests list || true

fail() { echo "FAIL: $1" >&2; cat "$T/opencode.err" >&2; exit 1; }
[ "$RC" -eq 0 ] || fail "opencode exit $RC"
grep -q 'POLICY_DONE saw=true' "$T/opencode.out" || fail "final text missing or tool output not seen"
grep -q POLICY_MARKER_9917 "$T/requests.json" || fail "tool output never reached the server"
echo "opencode policy e2e ok"
