#!/bin/sh
set -eu

IMAGE=${IMAGEGEN_BRIDGE_SMOKE_IMAGE:-imagegen-bridge:smoke}
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TEMP=$(mktemp -d)
NAME="imagegen-bridge-smoke-$$"
SEEDER="${NAME}-seed"
CONFIG_VOLUME="${NAME}-config"
WORKSPACE_VOLUME="${NAME}-workspace"
STATE_VOLUME="${NAME}-state"
ARTIFACT_VOLUME="${NAME}-artifacts"
CODEX_VOLUME="${NAME}-codex-home"
NETWORK="${NAME}-network"
CLIENT_CONTAINER=""
SMOKE_TOKEN="container-smoke-test-token"

cleanup() {
  status=$?
  if [ "$status" -ne 0 ]; then
    docker logs --tail 100 "$NAME" >&2 2>/dev/null || true
  fi
  docker rm -f "$SEEDER" >/dev/null 2>&1 || true
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  if [ -n "$CLIENT_CONTAINER" ]; then
    docker network disconnect -f "$NETWORK" "$CLIENT_CONTAINER" >/dev/null 2>&1 || true
  fi
  docker network rm "$NETWORK" >/dev/null 2>&1 || true
  docker volume rm -f \
    "$CONFIG_VOLUME" "$WORKSPACE_VOLUME" "$STATE_VOLUME" \
    "$ARTIFACT_VOLUME" "$CODEX_VOLUME" >/dev/null 2>&1 || true
  rm -rf "$TEMP"
  return "$status"
}
trap cleanup EXIT INT TERM

if [ "${IMAGEGEN_BRIDGE_SKIP_BUILD:-0}" != "1" ]; then
  docker build --tag "$IMAGE" "$ROOT"
fi

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

for volume in \
  "$CONFIG_VOLUME" "$WORKSPACE_VOLUME" "$STATE_VOLUME" \
  "$ARTIFACT_VOLUME" "$CODEX_VOLUME"
do
  docker volume create "$volume" >/dev/null
done
docker network create "$NETWORK" >/dev/null

# A containerized self-hosted runner cannot reach ports published on its own
# loopback. Join the disposable test network when the Docker daemon recognizes
# this client's hostname; ordinary host clients continue through localhost.
if [ -n "${HOSTNAME:-}" ] && docker inspect "$HOSTNAME" >/dev/null 2>&1; then
  docker network connect "$NETWORK" "$HOSTNAME"
  CLIENT_CONTAINER=$HOSTNAME
fi

# Seed through the Docker API instead of bind-mounting client-side temporary
# paths. This also works when the test client itself uses a host Docker socket.
docker create --name "$SEEDER" --user 0 --entrypoint /bin/sh \
  --volume "$CONFIG_VOLUME:/config" \
  --volume "$WORKSPACE_VOLUME:/workspace" \
  --volume "$STATE_VOLUME:/data/state" \
  --volume "$ARTIFACT_VOLUME:/data/artifacts" \
  --volume "$CODEX_VOLUME:/codex-home" \
  "$IMAGE" -c '
    chmod 0755 /workspace/fake-codex &&
    chown -R 10001:10001 /data/state /data/artifacts /codex-home
  ' >/dev/null
docker cp "$TEMP/config.toml" "$SEEDER:/config/imagegen-bridge.toml"
docker cp "$TEMP/fake-codex" "$SEEDER:/workspace/fake-codex"
docker start --attach "$SEEDER" >/dev/null
docker rm "$SEEDER" >/dev/null

docker run --detach --name "$NAME" \
  --network "$NETWORK" \
  --read-only \
  --cap-drop ALL \
  --security-opt no-new-privileges:true \
  --pids-limit 128 \
  --tmpfs /tmp:rw,noexec,nosuid,nodev,size=64m,uid=10001,gid=10001,mode=1777 \
  --tmpfs /home/imagegen:rw,noexec,nosuid,nodev,size=16m,uid=10001,gid=10001,mode=0700 \
  --env IMAGEGEN_BRIDGE_BEARER_TOKEN="$SMOKE_TOKEN" \
  --publish 127.0.0.1::8787 \
  --volume "$CONFIG_VOLUME:/config:ro" \
  --volume "$CODEX_VOLUME:/codex-home" \
  --volume "$WORKSPACE_VOLUME:/workspace:ro" \
  --volume "$STATE_VOLUME:/data/state" \
  --volume "$ARTIFACT_VOLUME:/data/artifacts" \
  "$IMAGE" >/dev/null

PORT=$(docker port "$NAME" 8787/tcp | sed -n 's/.*://p')
BASE_URL="http://127.0.0.1:$PORT"
if [ -n "$CLIENT_CONTAINER" ]; then
  BASE_URL="http://$NAME:8787"
fi
attempt=0
until curl --fail --silent "$BASE_URL/health/live" >/dev/null; do
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

curl --fail --silent "$BASE_URL/health/ready" | grep -q '"status":"ready"'
curl --fail --silent \
  --header "Authorization: Bearer $SMOKE_TOKEN" \
  "$BASE_URL/metrics" | grep -q '^# HELP imagegen_bridge_requests_total'

STATUS=$(curl --silent --output /dev/null --write-out '%{http_code}' \
  "$BASE_URL/v1/providers")
[ "$STATUS" = "401" ]

docker stop --time 45 "$NAME" >/dev/null
[ "$(docker inspect --format '{{.State.ExitCode}}' "$NAME")" = "0" ]
