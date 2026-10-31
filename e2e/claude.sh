#!/usr/bin/env bash
# Smoke test: the real `claude` CLI (Claude Code) against fakellm's Anthropic Messages endpoint.
# Everything lives in /tmp/fakellm-e2e-claude; the user's ~/.claude is never touched.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
work=/tmp/fakellm-e2e-claude
rm -rf "$work"; mkdir -p "$work/config" "$work/cwd"
cargo build --manifest-path "$root/Cargo.toml" -q
"$root/target/debug/fakellm" --port 0 >"$work/server.out" 2>"$work/server.err" &
pid=$!
trap 'kill $pid 2>/dev/null || true' EXIT
for _ in $(seq 50); do url=$(grep -Eo 'http://[0-9.]+:[0-9]+' "$work/server.out" | head -1 || true); [ -n "$url" ] && break; sleep 0.1; done
curl -sf -X PUT "$url/_fakellm/scripts" -H 'content-type: application/json' \
  -d '{"rules":[{"name":"any","reply":{"text":"FAKELLM_OK"}}]}' >/dev/null
cd "$work/cwd"
env CLAUDE_CONFIG_DIR="$work/config" ANTHROPIC_BASE_URL="$url" ANTHROPIC_API_KEY=fakellm \
  DISABLE_TELEMETRY=1 DISABLE_AUTOUPDATER=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
  timeout 120 claude -p "say hi" --output-format json --no-session-persistence \
  --permission-mode bypassPermissions >"$work/result.json"
cat "$work/result.json"; echo
grep -q FAKELLM_OK "$work/result.json"
curl -s "$url/_fakellm/requests" >"$work/requests.json"
echo "request log: $work/requests.json"
echo PASS
