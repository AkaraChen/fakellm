#!/usr/bin/env bash
# Real-harness fault checks: run the codex CLI against fakellm with injected Responses API errors.
# Expected outputs below were observed with codex-cli 0.162.1; the script exits non-zero if they change.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${FAKELLM_BIN:-$ROOT/target/debug/fakellm}"
WORK=/tmp/fakellm-e2e-codex-faults
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

# Runs codex expecting a non-zero exit; stderr lands in $WORK/$2.err.
run_codex_fail() {
  if CODEX_HOME="$WORK/home" timeout "${CODEX_TIMEOUT:-120}" codex exec --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox \
    -C "$WORK/cwd" "$1" </dev/null >"$WORK/$2.out" 2>"$WORK/$2.err"; then
    echo "$2: codex exited 0, expected failure" >&2
    exit 1
  fi
}

# Scenario 1: context_length_exceeded as a response.failed event (in_stream). codex maps it to a fixed message and does not retry.
curl -sf -X PUT "$URL/_fakellm/scripts" \
  -d '[{"reply":{"error":{"status":400,"code":"context_length_exceeded","in_stream":true}}}]' >/dev/null
run_codex_fail "hi" s1
grep -q "Codex ran out of room in the model's context window" "$WORK/s1.err" \
  || { echo "s1: context window message missing" >&2; cat "$WORK/s1.err" >&2; exit 1; }
n="$(curl -sf "$URL/_fakellm/requests" | grep -o '"path":"/v1/responses"' | wc -l)"
[ "$n" -eq 1 ] || { echo "s1: expected 1 request (no retry), got $n" >&2; exit 1; }

# Scenario 2: the same error as a plain HTTP 400. codex prints the raw error body instead of the friendly message.
curl -sf -X DELETE "$URL/_fakellm/requests" >/dev/null
curl -sf -X PUT "$URL/_fakellm/scripts" \
  -d '[{"reply":{"error":{"status":400,"code":"context_length_exceeded"}}}]' >/dev/null
run_codex_fail "hi" s2
grep -q '"code":"context_length_exceeded"' "$WORK/s2.err" \
  || { echo "s2: raw error body missing" >&2; cat "$WORK/s2.err" >&2; exit 1; }
if grep -q "ran out of room" "$WORK/s2.err"; then echo "s2: unexpected friendly message" >&2; exit 1; fi

reset_requests() { curl -sf -X DELETE "$URL/_fakellm/requests" >/dev/null; }
put_rules() { curl -sf -X PUT "$URL/_fakellm/scripts" -d "$1" >/dev/null; reset_requests; }
fail() { echo "FAIL: $1" >&2; exit 1; }
count_responses() { { curl -sf "$URL/_fakellm/requests" | grep -o '"path":"/v1/responses"' || true; } | wc -l; }
expect_err() { grep -q "$2" "$WORK/$1.err" || { echo "$1: '$2' missing from stderr" >&2; cat "$WORK/$1.err" >&2; exit 1; }; }
expect_requests() { local n; n=$(count_responses); [ "$n" -eq "$2" ] || fail "$1: expected $2 requests, got $n"; }

# Scenario 3: insufficient_quota as response.failed. A fixed message, no retry.
put_rules '[{"reply":{"error":{"status":429,"type":"insufficient_quota","code":"insufficient_quota","in_stream":true}}}]'
run_codex_fail "hi" s3
expect_err s3 "Quota exceeded. Check your plan and billing details."
expect_requests s3 1

# Scenario 4: rate_limit_exceeded with error.headers, stream_max_retries = 0: the error is surfaced after one request.
put_rules '[{"reply":{"error":{"status":429,"code":"rate_limit_exceeded","in_stream":true,"retry_after":1,"headers":{"retry-after-ms":"200"}}}}]'
run_codex_fail "hi" s4
expect_err s4 "rate limit exceeded: Rate limit reached for requests"
expect_requests s4 1

# Scenario 5: response.incomplete with max_output_tokens (rule finish_reason: length). The partial text is printed, then
# codex reports a hard error and does not retry.
put_rules '[{"reply":{"text":"PARTIAL_TEXT","finish_reason":"length"}}]'
run_codex_fail "hi" s5
grep -q PARTIAL_TEXT "$WORK/s5.err" || fail "s5: partial text missing"
expect_err s5 "Incomplete response returned, reason: max_output_tokens"
expect_requests s5 1

# Scenario 6: overloaded without retry-after is a plain error ("Selected model is at capacity"), no retry.
put_rules '[{"reply":{"error":{"status":503,"in_stream":true}}}]'
run_codex_fail "hi" s6
expect_err s6 "Selected model is at capacity"
expect_requests s6 1

# Scenarios 7-8 enable codex's stream retries (stream_max_retries = 2) to see which failures it retries.
cp "$WORK/home/config.toml" "$WORK/config.noretry.toml"
sed -i 's/stream_max_retries = 0/stream_max_retries = 2/' "$WORK/home/config.toml"
run_codex_ok() {
  CODEX_HOME="$WORK/home" timeout 120 codex exec --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox \
    -C "$WORK/cwd" "$1" </dev/null >"$WORK/$2.out" 2>"$WORK/$2.err" || { echo "codex failed ($2)" >&2; cat "$WORK/$2.err" >&2; exit 1; }
}

# Scenario 7: rate_limit_exceeded once, then success: codex prints "Reconnecting... 1/2", retries, and answers.
put_rules '[{"times":1,"reply":{"error":{"status":429,"code":"rate_limit_exceeded","in_stream":true,"retry_after":1,"headers":{"retry-after-ms":"200"}}}},{"reply":{"text":"RETRIED_OK"}}]'
run_codex_ok "hi" s7
grep -q RETRIED_OK "$WORK/s7.out" || fail "s7: final text missing"
expect_err s7 "Reconnecting... 1/2"
expect_requests s7 2

# Scenario 8: server_is_overloaded with retry_after is retried (without it, scenario 6).
put_rules '[{"times":1,"reply":{"error":{"status":503,"retry_after":1,"in_stream":true}}},{"reply":{"text":"RETRIED_OK"}}]'
run_codex_ok "hi" s8
grep -q RETRIED_OK "$WORK/s8.out" || fail "s8: final text missing"
expect_requests s8 2
cp "$WORK/config.noretry.toml" "$WORK/home/config.toml"

# Scenario 9: apply_patch. A tool_calls rule naming apply_patch with a patch string comes back as a custom_tool_call;
# codex applies the patch (the file appears) and sends custom_tool_call_output.
PATCH=$'*** Begin Patch\n*** Add File: patched.txt\n+PATCHED_MARKER\n*** End Patch\n'
RULES=$(PATCH="$PATCH" python3 -c '
import json, os
print(json.dumps([
  {"match": {"has_tool_result": True}, "reply": {"text": "PATCH_DONE"}},
  {"reply": {"tool_calls": [{"name": "apply_patch", "arguments": os.environ["PATCH"]}]}}]))')
put_rules "$RULES"
run_codex_ok "patch it" s9
grep -q PATCH_DONE "$WORK/s9.out" || fail "s9: PATCH_DONE missing"
grep -q PATCHED_MARKER "$WORK/cwd/patched.txt" || fail "s9: patch not applied"
expect_requests s9 2
curl -sf "$URL/_fakellm/requests?full=true" | grep -q custom_tool_call_output || fail "s9: no custom_tool_call_output in the second request"

# Scenarios 10-12: hangs against stream_idle_timeout_ms = 5000, with the server clock frozen so the hang never ends by itself.
curl -sf -X POST "$URL/_fakellm/clock/freeze" >/dev/null

# 10: stream.ttft_ms sends the response headers first, then stays silent: codex gives up after the 5 s idle timeout.
put_rules '[{"reply":{"text":"LATE_OK","stream":{"ttft_ms":30000}}}]'
run_codex_fail "hi" s10
expect_err s10 "idle timeout waiting for SSE"
expect_requests s10 1

# 11: released by the clock within the idle window: codex does not notice.
put_rules '[{"reply":{"text":"LATE_OK","stream":{"ttft_ms":30000}}}]'
( sleep 2; curl -sf -X POST "$URL/_fakellm/clock/advance" -d '{"by":"30s"}' >/dev/null ) &
run_codex_ok "hi" s11
wait $! 2>/dev/null || true
grep -q LATE_OK "$WORK/s11.out" || fail "s11: LATE_OK missing"

# 12: hang_ms holds back the response headers. codex has no timeout for that phase: it waits until killed (timeout, exit 124).
put_rules '[{"reply":{"text":"LATE_OK","hang_ms":30000}}]'
CODEX_TIMEOUT=15 run_codex_fail "hi" s12
! grep -q "idle timeout" "$WORK/s12.err" || fail "s12: unexpected idle timeout; codex does have a header timeout now"

# 13: silence in the middle of the stream (tokens_per_sec with a frozen clock) is the same idle timeout.
put_rules '[{"reply":{"text":"one two three four five","stream":{"tokens_per_sec":1}}}]'
run_codex_fail "hi" s13
expect_err s13 "idle timeout waiting for SSE"
curl -sf -X POST "$URL/_fakellm/clock/reset" >/dev/null
echo "codex faults e2e ok"
