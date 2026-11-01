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
  if CODEX_HOME="$WORK/home" timeout 120 codex exec --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox \
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
echo "codex faults e2e ok"
