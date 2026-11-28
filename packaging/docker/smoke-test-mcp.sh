#!/usr/bin/env bash
# Start the web MCP image (Dockerfile.mcp, docs/web-mcp.md) and check that it serves the app with
# its MCP link compiled in, and that MCP answers with the token and refuses without it.
#
# Usage: packaging/docker/smoke-test-mcp.sh [IMAGE]     (default: solidcraft-web-mcp:test)
#
# Runs the container locked down as compose.mcp.yaml does. Used by .github/workflows/fork-image.yml.
set -euo pipefail

IMAGE="${1:-solidcraft-web-mcp:test}"
NAME="solidcraft-mcp-smoke-$$"
TOKEN="smoke-$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"
docker run -d --rm --name "$NAME" --read-only --tmpfs /tmp --cap-drop ALL \
  --security-opt no-new-privileges:true -e SOLIDCRAFT_WEB_MCP_TOKEN="$TOKEN" -p 127.0.0.1::8080 "$IMAGE" >/dev/null
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true' EXIT
PORT="$(docker port "$NAME" 8080/tcp | head -n1 | sed 's/.*://')"
BASE="http://127.0.0.1:$PORT"

fail() { echo "error: $*" >&2; docker logs "$NAME" >&2 || true; exit 1; }

for _ in $(seq 1 30); do
  curl -fs -o /dev/null "$BASE/healthz" && break
  sleep 1
done
[ "$(curl -fs "$BASE/healthz")" = ok ] || fail "/healthz does not answer ok"
docker exec "$NAME" solidcraft-web-mcp --healthcheck || fail "the image's own health check fails"

curl -fs "$BASE/" | grep -q 'solidcraft_canvas' || fail "index.html is not the app"
WASM="$(curl -fs "$BASE/" | grep -o 'solidcraft-web-[0-9a-f]\{8,16\}_bg\.wasm' | head -n1)"
[ -n "$WASM" ] || fail "index.html names no wasm"
curl -fs -o "$NAME.wasm" "$BASE/$WASM" || fail "$WASM is not served"
grep -aq 'mcp-link' "$NAME.wasm" || fail "$WASM was built without the mcp-link feature"
rm -f "$NAME.wasm"

code="$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' "$BASE/mcp")"
[ "$code" = 401 ] || fail "MCP without the token answers $code, not 401"

hello='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke-test","version":"0"}}}'
reply="$(curl -fs -X POST -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' -d "$hello" "$BASE/mcp")" || fail "MCP with the token refused initialize"
echo "$reply" | grep -q '"serverInfo"' || fail "MCP's initialize reply has no serverInfo: $reply"

status="$(curl -fs "$BASE/mcp-link/status")" || fail "/mcp-link/status does not answer"
echo "smoke test passed: $IMAGE serves $WASM with mcp-link; MCP answers initialize; link status $status"
