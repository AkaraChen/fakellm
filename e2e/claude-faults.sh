#!/usr/bin/env bash
# Real-harness checks: Claude Code (claude -p) against fakellm with a Bash tool loop and injected Anthropic errors.
# Observed with Claude Code 2.1.314; the script exits non-zero if the observed behaviour changes.
# Everything lives in /tmp/fakellm-e2e-claude-faults; the user's ~/.claude is never touched.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${FAKELLM_BIN:-$ROOT/target/debug/fakellm}
WORK=/tmp/fakellm-e2e-claude-faults
rm -rf "$WORK"
mkdir -p "$WORK/config" "$WORK/cwd"

"$BIN" --port 0 >"$WORK/server.out" 2>"$WORK/server.err" &
SERVER=$!
trap 'kill $SERVER 2>/dev/null || true' EXIT
URL=""
for _ in $(seq 100); do
  URL=$(grep -Eo 'http://[0-9.]+:[0-9]+' "$WORK/server.out" | head -1 || true)
  [ -n "$URL" ] && break
  sleep 0.05
done
[ -n "$URL" ] || { echo "fakellm did not start" >&2; exit 1; }

fail() { echo "FAIL: $1" >&2; exit 1; }

put_rules() { curl -sf -X PUT "$URL/_fakellm/scripts" -H 'content-type: application/json' -d "$1" >/dev/null; }

# Runs claude with a prompt; sets RC and leaves stdout/stderr in $WORK/$1.{out,err}. Never aborts the script.
run_claude() {
  local name=$1 prompt=$2
  curl -sf -X DELETE "$URL/_fakellm/requests" >/dev/null
  set +e
  (cd "$WORK/cwd" && env CLAUDE_CONFIG_DIR="$WORK/config" ANTHROPIC_BASE_URL="$URL" ANTHROPIC_API_KEY=fakellm \
    DISABLE_TELEMETRY=1 DISABLE_AUTOUPDATER=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
    timeout 180 claude -p "$prompt" --output-format json --no-session-persistence \
    --permission-mode bypassPermissions </dev/null >"$WORK/$name.out" 2>"$WORK/$name.err")
  RC=$?
  set -e
  curl -s "$URL/_fakellm/requests?full=true" >"$WORK/$name.requests.json" || true
}

# Number of logged requests to the given path prefix.
count_requests() { { curl -s "$URL/_fakellm/requests" | grep -o "\"path\":\"$1[^\"]*\"" || true; } | wc -l; }

# The loop rules: tool result back -> final text, otherwise ask for Bash.
LOOP='{"name":"done","match":{"has_tool_result":true},"reply":{"text":"CLAUDE_LOOP_DONE"}},
 {"name":"bash","reply":{"tool_calls":[{"name":"Bash","arguments":{"command":"echo CLAUDE_FAULT_MARKER > made.txt && cat made.txt","description":"write marker"}}]}}'

# Scenario 0: the tool loop with no faults. Bash runs, the final text arrives, exactly two /v1/messages requests.
put_rules "[$LOOP]"
run_claude s0 "make the file"
[ "$RC" -eq 0 ] || { cat "$WORK/s0.err" >&2; fail "s0: claude exit $RC"; }
grep -q CLAUDE_LOOP_DONE "$WORK/s0.out" || fail "s0: final text missing"
grep -q CLAUDE_FAULT_MARKER "$WORK/cwd/made.txt" || fail "s0: Bash did not run"
n=$(count_requests /v1/messages); [ "$n" -eq 2 ] || fail "s0: expected 2 requests, got $n"
echo "s0 tool loop: exit 0, $n requests"

# Fault scenarios: a times:1 error rule in front of the loop. The retry (or the next request) lands on the loop.
# fault NAME ERROR_JSON EXPECT_REQUESTS (total /v1/messages requests including the retry)
fault() {
  local name=$1 err=$2 expect=$3
  rm -f "$WORK/cwd/made.txt"
  put_rules "[{\"name\":\"fault\",\"times\":1,\"reply\":{\"error\":$err}},$LOOP]"
  local t0=$SECONDS
  run_claude "$name" "make the file"
  local n; n=$(count_requests /v1/messages)
  echo "$name: exit $RC, $n requests, $((SECONDS - t0))s, result: $(grep -o '"result":"[^"]*"' "$WORK/$name.out" | head -1)"
  [ "$RC" -eq 0 ] || { cat "$WORK/$name.err" >&2; fail "$name: claude exit $RC"; }
  grep -q CLAUDE_LOOP_DONE "$WORK/$name.out" || fail "$name: final answer missing"
  grep -q CLAUDE_FAULT_MARKER "$WORK/cwd/made.txt" || fail "$name: Bash did not run"
  [ "$n" -eq "$expect" ] || fail "$name: expected $expect requests, got $n"
}

fault s1-429 '{"status":429,"retry_after":1}' 3
fault s2-500 '{"status":500}' 3
fault s3-529 '{"status":529}' 3
fault s4-stream-error '{"status":529,"in_stream":true}' 3

# Non-retryable-looking client errors: claude exits 1 and prints the API error as the result.
# fail_case NAME ERROR_JSON EXPECT_REQUESTS EXPECT_TEXT
fail_case() {
  local name=$1 err=$2 expect=$3 text=$4
  put_rules "[{\"name\":\"fault\",\"reply\":{\"error\":$err}}]"
  run_claude "$name" "hi"
  local n; n=$(count_requests /v1/messages)
  echo "$name: exit $RC, $n requests, result: $(grep -o '"result":"[^"]*"' "$WORK/$name.out" | head -1)"
  [ "$RC" -eq 1 ] || fail "$name: expected exit 1, got $RC"
  grep -q "$text" "$WORK/$name.out" || fail "$name: '$text' missing from the result"
  [ "$n" -eq "$expect" ] || fail "$name: expected $expect requests, got $n"
}

fail_case s5-400 '{"status":400}' 6 'is_error":true'
fail_case s6-404 '{"status":404}' 2 'is_error":true'
# 401 is not run: claude retries it for far longer than 150 s (10+ attempts) and has to be killed by the timeout.

# HEAD /api/hello: Claude Code's connectivity preconnect (a fire-and-forget HEAD with a 10 s timeout, only for the
# first-party provider without a custom base URL, proxy or the nonessential-traffic switch). With ANTHROPIC_BASE_URL
# pointing at fakellm, claude 2.1.314 never sends it, whether or not CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC is set.
echo "-- HEAD /api/hello"
put_rules "[$LOOP]"
run_claude s7-hello "make the file"
[ "$RC" -eq 0 ] || fail "s7: claude exit $RC"
n=$({ grep -o /api/hello "$WORK/s7-hello.requests.json" || true; } | wc -l); [ "$n" -eq 0 ] || fail "s7: claude sent HEAD /api/hello"
# Unknown paths are 404 and not logged. If a client does probe it, a custom endpoint rule answers (and logs) it.
code=$(curl -s -o /dev/null -w '%{http_code}' -I "$URL/api/hello"); [ "$code" = 404 ] || fail "HEAD without a rule: $code"
put_rules '[{"match":{"path":"/api/hello"},"reply":{"body":{}}}]'
code=$(curl -s -o /dev/null -w '%{http_code}' -I "$URL/api/hello"); [ "$code" = 200 ] || fail "HEAD with a rule: $code"
curl -s "$URL/_fakellm/requests" | grep -q '"method":"HEAD","path":"/api/hello"' || fail "HEAD /api/hello not logged"
echo "claude e2e faults ok"
