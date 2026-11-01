#!/usr/bin/env bash
# Image smoke test: builds both images (fakellm:local and the node variant fakellm:node-local), runs each on a random
# host port and curls a chat completion through it. The node variant must also run a policy script; the default image
# must refuse one with a message that names the missing JavaScript runtime.
#
# FAKELLM_IMAGE / FAKELLM_NODE_IMAGE test prebuilt images instead of building (either one can be set alone).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="${FAKELLM_IMAGE:-fakellm:local}"
NODE_IMAGE="${FAKELLM_NODE_IMAGE:-fakellm:node-local}"
[ -n "${FAKELLM_IMAGE:-}" ] || docker build -t "$IMAGE" "$ROOT"
[ -n "${FAKELLM_NODE_IMAGE:-}" ] || docker build -f "$ROOT/Dockerfile.node" -t "$NODE_IMAGE" "$ROOT"

TMPF="$(mktemp)"
CID=""
trap 'rm -f "$TMPF"; [ -z "$CID" ] || docker rm -f "$CID" >/dev/null 2>&1 || true' EXIT

# Starts $1, sets CID and PORT.
start() {
  CID="$(docker run -d -p 127.0.0.1:0:4010 "$1")"
  for _ in $(seq 100); do
    docker logs "$CID" 2>&1 | grep -q '^http://' && break
    sleep 0.1
  done
  PORT="$(docker port "$CID" 4010/tcp | head -n1 | sed 's/.*://')"
}

chat() {
  curl -sf -H 'content-type: application/json' \
    -d '{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}' "http://127.0.0.1:$PORT/v1/chat/completions"
}

POLICY='{"name":"p","source":"export default () => ({ text: \"hello from a policy\" });"}'
put_policy() {
  curl -s -o "$TMPF" -w '%{http_code}' -X PUT -H 'content-type: application/json' \
    -d "$POLICY" "http://127.0.0.1:$PORT/_fakellm/policies"
}

# Default image: chat works, policies are refused with a clear message.
start "$IMAGE"
BODY="$(chat)"
echo "$BODY" | grep -q '"chat.completion"' || { echo "FAIL: $BODY" >&2; exit 1; }
CODE="$(put_policy)"
[ "$CODE" = 400 ] || { echo "FAIL: policy on the default image answered $CODE" >&2; exit 1; }
grep -q 'JavaScript runtime' "$TMPF" || { echo "FAIL: unclear policy error: $(cat "$TMPF")" >&2; exit 1; }
echo "PASS docker ($IMAGE on port $PORT, $(docker image inspect -f '{{.Size}}' "$IMAGE") bytes; policies refused: no JS runtime)"
docker rm -f "$CID" >/dev/null; CID=""

# Node variant: chat works and a policy decides a reply.
start "$NODE_IMAGE"
BODY="$(chat)"
echo "$BODY" | grep -q '"chat.completion"' || { echo "FAIL: $BODY" >&2; exit 1; }
CODE="$(put_policy)"
[ "$CODE" = 200 ] || { echo "FAIL: policy on the node image answered $CODE: $(cat "$TMPF")" >&2; exit 1; }
curl -sf -X PUT -H 'content-type: application/json' -d '[{"reply":{"policy":"p"}}]' "http://127.0.0.1:$PORT/_fakellm/scripts" >/dev/null
BODY="$(chat)"
echo "$BODY" | grep -q 'hello from a policy' || { echo "FAIL: $BODY" >&2; exit 1; }
rm -f "$TMPF"
echo "PASS docker node ($NODE_IMAGE on port $PORT, $(docker image inspect -f '{{.Size}}' "$NODE_IMAGE") bytes; policy ran)"
