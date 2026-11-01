#!/usr/bin/env bash
# Does a CONNECT proxy forward fakellm's RST (reset_after_chunks) or turn it into a FIN? Runs the raw client through
# e2e/connect-proxy.mjs in both modes, and directly (no proxy) as the baseline. Needs node.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${FAKELLM_BIN:-$ROOT/target/debug/fakellm}
HERE=$ROOT/e2e
T=/tmp/fakellm-e2e-connect-rst
rm -rf "$T"; mkdir -p "$T"

"$BIN" --port 0 >"$T/server.out" 2>"$T/server.err" &
SERVER=$!
PROXIES=()
trap 'kill $SERVER "${PROXIES[@]}" 2>/dev/null || true' EXIT
URL=""
for _ in $(seq 100); do
  URL=$(grep -Eo 'http://[0-9.]+:[0-9]+' "$T/server.out" | head -1 || true)
  [ -n "$URL" ] && break
  sleep 0.05
done
[ -n "$URL" ] || { echo "fakellm did not start" >&2; exit 1; }
PORT=${URL##*:}
curl -sf -X PUT "$URL/_fakellm/scripts" -d '[{"reply":{"text":"one two three four five six","stream":{"reset_after_chunks":2}}}]' >/dev/null

start_proxy() {
  node "$HERE/connect-proxy.mjs" "$1" >"$T/proxy-$1.out" 2>&1 &
  PROXIES+=($!)
  local p=""
  for _ in $(seq 100); do
    p=$(grep -o 'listening [0-9]*' "$T/proxy-$1.out" 2>/dev/null | cut -d' ' -f2 || true)
    [ -n "$p" ] && break
    sleep 0.05
  done
  [ -n "$p" ] || { echo "proxy $1 did not start" >&2; exit 1; }
  echo "$p"
}

# Baseline: the same request straight at fakellm, no proxy. The client must see ECONNRESET.
direct=$(node -e '
const net=require("node:net");let body="";const s=net.connect(+process.argv[1],"127.0.0.1");
const p=JSON.stringify({model:"m",stream:true,messages:[{role:"user",content:"hi"}]});
s.on("connect",()=>s.write(`POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: ${p.length}\r\n\r\n${p}`));
s.on("data",d=>body+=d.toString("latin1"));
s.on("error",e=>{console.log("RESULT "+(e.code==="ECONNRESET"?"reset":e.code));process.exit(0)});
s.on("end",()=>{console.log("RESULT eof");process.exit(0)});
setTimeout(()=>{console.log("RESULT timeout");process.exit(0)},15000).unref();' "$PORT")
echo "direct (no proxy): $direct"
[ "$direct" = "RESULT reset" ] || { echo "FAIL: baseline is not a reset" >&2; exit 1; }

for mode in naive rst; do
  pp=$(start_proxy "$mode")
  out=$(node "$HERE/connect-client.mjs" "$pp" "$PORT")
  echo "via $mode proxy: $out ($(grep -c 'upstream error' "$T/proxy-$mode.out") upstream error(s) seen by the proxy)"
  echo "$out" >"$T/result-$mode"
done
[ "$(cat "$T/result-naive")" = "RESULT eof incomplete" ] || { echo "FAIL: naive proxy result changed" >&2; exit 1; }
[ "$(cat "$T/result-rst")" = "RESULT reset incomplete" ] || { echo "FAIL: rst proxy result changed" >&2; exit 1; }
echo "connect rst e2e ok"
