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
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"id":%s,"result":{}}\n' "$id"
      ;;
    *'"method":"account/read"'*)
      printf '{"id":%s,"result":{"account":{"type":"chatgpt"}}}\n' "$id"
      ;;
    *'"method":"thread/start"'*)
      printf '%s\n' 'thread/start' >>/data/state/rpc-methods.log
      printf '{"id":%s,"result":{"thread":{"id":"thread-container-smoke"}}}\n' "$id"
      ;;
    *'"method":"thread/resume"'*)
      printf '%s\n' 'thread/resume' >>/data/state/rpc-methods.log
      printf '{"id":%s,"result":{"thread":{"id":"thread-container-smoke"}}}\n' "$id"
      ;;
    *'"method":"turn/start"'*)
      printf '{"id":%s,"result":{"turn":{"id":"turn-container-smoke"}}}\n' "$id"
      printf '%s\n' '{"method":"item/completed","params":{"threadId":"thread-container-smoke","turnId":"turn-container-smoke","item":{"type":"imageGeneration","id":"image-container-smoke","status":"completed","revisedPrompt":"container smoke fixture","result":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII="}}}'
      printf '%s\n' '{"method":"turn/completed","params":{"threadId":"thread-container-smoke","turn":{"id":"turn-container-smoke","status":"completed","items":[]}}}'
      ;;
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

[server.jobs]
enabled = true
database = "/data/state/jobs.sqlite3"
EOF

cat >"$TEMP/request-first.json" <<'EOF'
{
  "version": "1",
  "prompt": "container restart fixture one",
  "operation": "generate",
  "session": {"mode": "persistent", "key": "container-smoke"},
  "policies": {"compatibility": "normalize"}
}
EOF
cat >"$TEMP/request-second.json" <<'EOF'
{
  "version": "1",
  "prompt": "container restart fixture two",
  "operation": "generate",
  "session": {"mode": "persistent", "key": "container-smoke"},
  "policies": {"compatibility": "normalize"}
}
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

# A deliberately sparse version-1 configuration models an older deployment.
# New defaults must stay backward-compatible, and validation must work with a
# read-only root filesystem without opening provider or SQLite state.
CHECK=$(docker run --rm \
  --read-only \
  --tmpfs /tmp:rw,noexec,nosuid,nodev,size=16m,uid=10001,gid=10001,mode=1777 \
  --tmpfs /home/imagegen:rw,noexec,nosuid,nodev,size=8m,uid=10001,gid=10001,mode=0700 \
  --volume "$CONFIG_VOLUME:/config:ro" \
  --volume "$WORKSPACE_VOLUME:/workspace:ro" \
  --volume "$STATE_VOLUME:/data/state" \
  --volume "$ARTIFACT_VOLUME:/data/artifacts" \
  "$IMAGE" imagegen-bridge --config /config/imagegen-bridge.toml config check --json)
[ "$CHECK" = '{"issues":[],"valid":true}' ]

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

curl --fail --silent --show-error \
  --header "Authorization: Bearer $SMOKE_TOKEN" \
  --header "Content-Type: application/json" \
  --data-binary "@$TEMP/request-first.json" \
  --output "$TEMP/first-response.json" \
  "$BASE_URL/v1/images"
grep -q '"thread_id":"thread-container-smoke"' "$TEMP/first-response.json"
grep -q '"reused":false' "$TEMP/first-response.json"

docker stop --time 45 "$NAME" >/dev/null
[ "$(docker inspect --format '{{.State.ExitCode}}' "$NAME")" = "0" ]

# Reuse the exact container and named volumes. The bridge must reopen its
# migrated SQLite state and resume the existing Codex thread rather than create
# another chat.
docker start "$NAME" >/dev/null
attempt=0
until curl --fail --silent "$BASE_URL/health/live" >/dev/null; do
  attempt=$((attempt + 1))
  if [ "$attempt" -ge 100 ]; then
    docker logs --tail 100 "$NAME" >&2
    exit 1
  fi
  sleep 0.1
done
curl --fail --silent "$BASE_URL/health/ready" | grep -q '"status":"ready"'
curl --fail --silent --show-error \
  --header "Authorization: Bearer $SMOKE_TOKEN" \
  --header "Content-Type: application/json" \
  --data-binary "@$TEMP/request-second.json" \
  --output "$TEMP/second-response.json" \
  "$BASE_URL/v1/images"
grep -q '"thread_id":"thread-container-smoke"' "$TEMP/second-response.json"
grep -q '"reused":true' "$TEMP/second-response.json"
docker exec "$NAME" sh -c \
  'grep -qx "thread/start" /data/state/rpc-methods.log && grep -qx "thread/resume" /data/state/rpc-methods.log'

docker stop --time 45 "$NAME" >/dev/null
[ "$(docker inspect --format '{{.State.ExitCode}}' "$NAME")" = "0" ]
