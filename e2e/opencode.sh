#!/usr/bin/env bash
# Real-harness smoke test: drive the opencode binary against fakellm.
# Everything lives under /tmp/fakellm-e2e-opencode; the user's opencode config and data are untouched.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${FAKELLM_BIN:-$ROOT/target/debug/fakellm}
T=/tmp/fakellm-e2e-opencode
rm -rf "$T"
mkdir -p "$T"/{project,home,config,data,cache,state}

"$BIN" --port 0 >"$T/server.out" 2>"$T/server.err" &
PID=$!
trap 'kill $PID 2>/dev/null || true' EXIT

for _ in $(seq 100); do
  PORT=$(grep -oE '127\.0\.0\.1:[0-9]+' "$T/server.out" | head -1 | cut -d: -f2 || true)
  [ -n "$PORT" ] && break
  sleep 0.1
done
[ -n "${PORT:-}" ] || { echo "fakellm did not start" >&2; cat "$T/server.err" >&2; exit 1; }
URL=http://127.0.0.1:$PORT
export FAKELLM_ENDPOINT=$URL

# Side requests (title generation) carry no tools and a "title" system prompt; the main turn is the catch-all.
cat >"$T/rules.json" <<'JSON'
[
  {"name": "title", "match": {"no_tools": true, "system": "(?i)title"}, "reply": {"text": "Greeting"}},
  {"name": "main", "match": {"any": true}, "reply": {"text": "FAKELLM_OK"}}
]
JSON
curl -sf -X PUT "$URL/_fakellm/scripts" -H 'content-type: application/json' --data @"$T/rules.json" >/dev/null

cat >"$T/project/opencode.json" <<JSON
{
  "\$schema": "https://opencode.ai/config.json",
  "model": "fakellm/fake-model",
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
  timeout 120 opencode run --model fakellm/fake-model "say hi" </dev/null >"$T/opencode.out" 2>"$T/opencode.err")
RC=$?
set -e

echo "== opencode exit $RC"; cat "$T/opencode.out"
echo "== request log"
"$BIN" ctl requests list || true
curl -s "$URL/_fakellm/requests" >"$T/requests.json" || true

grep -q FAKELLM_OK "$T/opencode.out" && echo "PASS" || { echo "FAIL"; cat "$T/opencode.err" >&2; exit 1; }
