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
| `/data/state` | SQLite session bindings and durable job history | read/write, persistent |
| `/data/artifacts` | Verified bridge-owned outputs | read/write, persistent |
| `/workspace` | Optional local/reference inputs | read-only |
| `/tmp`, `/home/imagegen` | Bounded ephemeral scratch | tmpfs |

## Health and shutdown

`GET /health/live` is public and content-free for container health checks.
`GET /health/ready` reads a detail-free cached provider snapshot. Provider
probes run on a bounded background cadence; full per-provider state remains in
the authenticated `GET /v1/diagnostics` response.
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

## Troubleshooting

If liveness succeeds but readiness returns `503`, keep the service on loopback
and run the non-generating diagnostics first:

```sh
docker compose exec imagegen-bridge imagegen-bridge \
  --config /config/imagegen-bridge.toml doctor --non-interactive --json
docker compose logs --tail 100 imagegen-bridge
```

An authentication failure normally means `/codex-home/auth.json` is absent,
expired, or not writable by UID 10001. Re-run `codex login` outside the
container, copy only the dedicated Codex state described above, preserve its
secret permissions, and restart. Do not print `auth.json` or mount a whole user
home while debugging.

For `permission denied` errors, verify ownership of the Codex, state, and
artifact mounts and that the config/workspace mounts are intentionally
read-only. Do not solve this with world-writable permissions. A config failure
should be reproduced with `config check`; it is non-mutating and reports the
field path without resolving secrets.

If startup reports a SQLite migration error, stop the service, take a
consistent backup of `/data/state`, and retain the original volume for
diagnosis. Never delete or hand-edit the database as an automatic repair.
Queued jobs resume after a clean restart; a job that was running during an
uncertain shutdown becomes `interrupted` and is not paid/retried again without
an explicit new request. Provider restart counts, readiness, bounded storage
facts, and redacted recent API events are available from authenticated
`GET /v1/diagnostics`.

## Container verification

`tests/container-smoke.sh` builds the image and runs a disposable fake-Codex
deployment. It verifies non-root UID, read-only rootfs, liveness, provider
readiness, protected metrics/API, backward-compatible version-1 config loading,
graceful SIGTERM exit, persistent SQLite state, and app-server thread resume
after a real container stop/start. The generated fixture is local and it never
performs a live or paid image generation.
