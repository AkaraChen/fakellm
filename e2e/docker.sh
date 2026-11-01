#!/usr/bin/env bash
# Image smoke test: build the Docker image, run it on a random host port and curl a chat completion through it.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="${FAKELLM_IMAGE:-fakellm:local}"
[ -n "${FAKELLM_IMAGE:-}" ] || docker build -t "$IMAGE" "$ROOT"

CID="$(docker run -d -p 127.0.0.1:0:4010 "$IMAGE")"
trap 'docker rm -f "$CID" >/dev/null 2>&1 || true' EXIT

for _ in $(seq 100); do
  docker logs "$CID" 2>&1 | grep -q '^http://' && break
  sleep 0.1
done
PORT="$(docker port "$CID" 4010/tcp | head -n1 | sed 's/.*://')"

BODY="$(curl -sf -H 'content-type: application/json' \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}' "http://127.0.0.1:$PORT/v1/chat/completions")"
echo "$BODY" | grep -q '"chat.completion"' || { echo "FAIL: $BODY" >&2; exit 1; }
echo "PASS docker ($IMAGE on port $PORT, $(docker image inspect -f '{{.Size}}' "$IMAGE") bytes)"
