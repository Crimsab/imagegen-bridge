# Deployment and operations

The OCI image contains the bridge binary and the pinned Codex CLI, runs as
UID/GID `10001`, and uses `tini` for signal forwarding and child reaping. The
runtime root filesystem is compatible with read-only mode. Only Codex OAuth
state, bridge session state, verified artifacts, and optional input workspace
are mounted.

## Prepare Codex OAuth state

Use a dedicated writable Codex home rather than mounting an unrelated user's
entire home directory. Codex may rotate OAuth credentials, so `auth.json` must
remain writable by UID 10001.

```sh
install -d -m 0700 ./deploy/codex-home
cp "$HOME/.codex/auth.json" ./deploy/codex-home/auth.json
chown -R 10001:10001 ./deploy/codex-home
```

Never commit this directory. The repository ignores `deploy/codex-home/`.

## Compose

```sh
export IMAGEGEN_BRIDGE_BEARER_TOKEN="use-a-secret-manager-or-random-value"
export IMAGEGEN_BRIDGE_CODEX_HOME="$PWD/deploy/codex-home"
docker compose up --build -d
docker compose ps
```

The included Compose file binds the API to host loopback by default, drops every
Linux capability, blocks privilege escalation, limits PIDs, mounts the root
filesystem read-only, and uses bounded tmpfs mounts. It uses Compose's default
project network. Do not bind the API publicly without bridge bearer
authentication and a trusted TLS reverse proxy.

The default layout is:

| Container path | Purpose | Access |
| --- | --- | --- |
| `/config/imagegen-bridge.toml` | Versioned bridge configuration | read-only |
| `/codex-home` | Dedicated Codex OAuth state | read/write, secret |
| `/data/state` | SQLite session bindings | read/write, persistent |
| `/data/artifacts` | Verified bridge-owned outputs | read/write, persistent |
| `/workspace` | Optional local/reference inputs | read-only |
| `/tmp`, `/home/imagegen` | Bounded ephemeral scratch | tmpfs |

## Health and shutdown

`GET /health/live` is public and content-free for container health checks.
`GET /health/ready` verifies configured provider authentication/readiness.
`GET /metrics` is enabled in the container profile and protected by the bridge
bearer token. Compose allows 45 seconds for SIGTERM-driven draining, provider
shutdown, SQLite completion, and Codex child termination before SIGKILL.

## Backups and upgrades

Back up `/data/state` only after stopping the service or by using a
SQLite-consistent snapshot. Artifacts can be backed up independently. Codex
OAuth state is a secret and should use encrypted backup storage. Before an
upgrade, run `imagegen-bridge config check` with the new binary/image, then
replace the container without deleting named volumes. The configuration loader
rejects unknown or invalid fields before mutating storage.

## Container verification

`tests/container-smoke.sh` builds the image and runs a disposable fake-Codex
deployment. It verifies non-root UID, read-only rootfs, liveness, provider
readiness, protected metrics/API, and graceful SIGTERM exit. It never performs
a live or paid image generation.
