#!/usr/bin/env bash
# Real-harness smoke test: drive the opencode binary against fakellm.
# Everything lives under /tmp/fakellm-e2e-opencode; the user's opencode config and data are untouched.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${FAKELLM_BIN:-$ROOT/target/debug/fakellm}
T=/tmp/fakellm-e2e-opencode
rm -rf "$T"
mkdir -p "$T/project"

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

fail() { echo "FAIL: $1" >&2; exit 1; }

# Config as a file (OPENCODE_CONFIG) and as the OPENCODE_CONFIG_CONTENT env var that `fakellm wire opencode` prints.
"$BIN" wire opencode --endpoint "$URL" --file opencode.json >"$T/project/opencode.json"
CONTENT=$("$BIN" wire opencode --endpoint "$URL" --format env | sed 's/^OPENCODE_CONFIG_CONTENT=//')
[ -n "$CONTENT" ] || fail "wire opencode printed no OPENCODE_CONFIG_CONTENT"

# run_opencode NAME TIMEOUT [ENV=VALUE...]: fresh XDG dirs every time, so each run is a first run. Sets RC.
run_opencode() {
  local name=$1 limit=$2; shift 2
  local d="$T/$name"
  rm -rf "$d"; mkdir -p "$d"/{home,config,data,cache,state}
  set +e
  (cd "$T/project" && env HOME="$d/home" XDG_CONFIG_HOME="$d/config" XDG_DATA_HOME="$d/data" XDG_CACHE_HOME="$d/cache" XDG_STATE_HOME="$d/state" \
    OPENCODE_DISABLE_MODELS_FETCH=1 OPENCODE_DISABLE_AUTOUPDATE=1 "$@" \
    timeout "$limit" opencode run --model fakellm/fake-model "say hi" </dev/null >"$d/out" 2>"$d/err")
  RC=$?
  set -e
}

# Scenario 1: config from a file.
curl -sf -X DELETE "$URL/_fakellm/requests" >/dev/null
run_opencode file 120 OPENCODE_CONFIG="$T/project/opencode.json"
echo "== file config: exit $RC"; cat "$T/file/out"
grep -q FAKELLM_OK "$T/file/out" || { cat "$T/file/err" >&2; fail "file config: FAKELLM_OK missing"; }
"$BIN" ctl requests list || true
curl -s "$URL/_fakellm/requests" >"$T/requests.json" || true

# Scenario 2: config from OPENCODE_CONFIG_CONTENT, no file. The title side request must still be caught by the title rule.
curl -sf -X DELETE "$URL/_fakellm/requests" >/dev/null
run_opencode content 120 OPENCODE_CONFIG_CONTENT="$CONTENT"
echo "== OPENCODE_CONFIG_CONTENT: exit $RC"; cat "$T/content/out"
grep -q FAKELLM_OK "$T/content/out" || { cat "$T/content/err" >&2; fail "env config: FAKELLM_OK missing"; }
curl -s "$URL/_fakellm/requests" | grep -q '"rule":"title"' || fail "env config: title side request not seen"

# Scenario 3: the first-run hang seen once (120 s, empty request log) never reproduced. Five runs, each with fresh XDG
# dirs and a 60 s limit; every one must finish. Set OPENCODE_FRESH_RUNS=0 to skip.
for i in $(seq "${OPENCODE_FRESH_RUNS:-5}"); do
  t0=$SECONDS
  run_opencode "fresh$i" 60 OPENCODE_CONFIG_CONTENT="$CONTENT"
  echo "fresh run $i: exit $RC in $((SECONDS - t0))s"
  [ "$RC" -eq 0 ] && grep -q FAKELLM_OK "$T/fresh$i/out" || { cat "$T/fresh$i/err" >&2; fail "fresh run $i failed (exit $RC)"; }
done
echo PASS
