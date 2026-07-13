#!/bin/sh
set -eu

IMAGE=${IMAGEGEN_BRIDGE_SMOKE_IMAGE:-imagegen-bridge:smoke}
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TEMP=$(mktemp -d)
NAME="imagegen-bridge-smoke-$$"
SMOKE_TOKEN="container-smoke-test-token"

cleanup() {
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  rm -rf "$TEMP"
}
trap cleanup EXIT INT TERM

if [ "${IMAGEGEN_BRIDGE_SKIP_BUILD:-0}" != "1" ]; then
  docker build --tag "$IMAGE" "$ROOT"
fi

mkdir -p "$TEMP/state" "$TEMP/artifacts" "$TEMP/codex-home" "$TEMP/workspace"
chmod 0777 "$TEMP/state" "$TEMP/artifacts" "$TEMP/codex-home"
cat >"$TEMP/fake-codex" <<'SCRIPT'
#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{}}' ;;
    *'"method":"account/read"'*) printf '%s\n' '{"id":2,"result":{"account":{"type":"chatgpt"}}}' ;;
  esac
done
SCRIPT
chmod 0755 "$TEMP/fake-codex"
cat >"$TEMP/config.toml" <<EOF
version = 1
default_provider = "codex-app-server"

[inputs]
local_roots = ["/workspace"]

[artifacts]
root = "/data/artifacts"

[providers.codex_app_server]
enabled = true
executable = "/workspace/fake-codex"
args = []
cwd = "/workspace"
session_database = "/data/state/sessions.sqlite3"
restart_backoff_ms = 0

[providers.codex_responses]
enabled = false

[server]
bind = "0.0.0.0:8787"
bearer_token_env = "IMAGEGEN_BRIDGE_BEARER_TOKEN"

[server.metrics]
enabled = true
EOF
cp "$TEMP/fake-codex" "$TEMP/workspace/fake-codex"

docker run --detach --name "$NAME" \
  --read-only \
  --cap-drop ALL \
  --security-opt no-new-privileges:true \
  --pids-limit 128 \
  --tmpfs /tmp:rw,noexec,nosuid,nodev,size=64m,uid=10001,gid=10001,mode=1777 \
  --tmpfs /home/imagegen:rw,noexec,nosuid,nodev,size=16m,uid=10001,gid=10001,mode=0700 \
  --env IMAGEGEN_BRIDGE_BEARER_TOKEN="$SMOKE_TOKEN" \
  --publish 127.0.0.1::8787 \
  --volume "$TEMP/config.toml:/config/imagegen-bridge.toml:ro" \
  --volume "$TEMP/codex-home:/codex-home" \
  --volume "$TEMP/workspace:/workspace:ro" \
  --volume "$TEMP/state:/data/state" \
  --volume "$TEMP/artifacts:/data/artifacts" \
  "$IMAGE" >/dev/null

PORT=$(docker port "$NAME" 8787/tcp | sed -n 's/.*://p')
attempt=0
until curl --fail --silent "http://127.0.0.1:$PORT/health/live" >/dev/null; do
  attempt=$((attempt + 1))
  if [ "$attempt" -ge 100 ]; then
    docker logs --tail 100 "$NAME" >&2
    exit 1
  fi
  sleep 0.1
done

[ "$(docker exec "$NAME" id -u)" = "10001" ]
if docker exec "$NAME" touch /rootfs-must-be-read-only >/dev/null 2>&1; then
  echo "container root filesystem is writable" >&2
  exit 1
fi

curl --fail --silent "http://127.0.0.1:$PORT/health/ready" | grep -q '"status":"ready"'
curl --fail --silent \
  --header "Authorization: Bearer $SMOKE_TOKEN" \
  "http://127.0.0.1:$PORT/metrics" | grep -q '^# HELP imagegen_bridge_requests_total'

STATUS=$(curl --silent --output /dev/null --write-out '%{http_code}' \
  "http://127.0.0.1:$PORT/v1/providers")
[ "$STATUS" = "401" ]

docker stop --time 45 "$NAME" >/dev/null
[ "$(docker inspect --format '{{.State.ExitCode}}' "$NAME")" = "0" ]
