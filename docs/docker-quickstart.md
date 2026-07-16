# Docker quickstart

The included Compose profile runs Imagegen Bridge as a non-root process with a
read-only root filesystem, dropped Linux capabilities, bounded temporary
storage, and a loopback host bind.

## Prepare the repository

```sh
git clone https://github.com/Crimsab/imagegen-bridge.git
cd imagegen-bridge
```

Create a dedicated Codex home. Codex may rotate its OAuth state, so this mount
must remain writable by UID `10001`.

```sh
install -d -m 0700 ./deploy/codex-home
cp "$HOME/.codex/auth.json" ./deploy/codex-home/auth.json
chown -R 10001:10001 ./deploy/codex-home
```

The directory is ignored by Git. Never commit `auth.json` or mount an unrelated
user home into the container.

## Start the service

```sh
export IMAGEGEN_BRIDGE_BEARER_TOKEN="$(openssl rand -hex 32)"
export IMAGEGEN_BRIDGE_CODEX_HOME="$PWD/deploy/codex-home"
docker compose up --build -d
docker compose ps
```

Check the public, detail-free health endpoints:

```sh
curl --fail http://127.0.0.1:8787/health/live
curl --fail http://127.0.0.1:8787/health/ready
```

## Make the first authenticated request

```sh
curl --fail --silent --show-error \
  -H "Authorization: Bearer $IMAGEGEN_BRIDGE_BEARER_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"version":"1","operation":"generate","prompt":"A paper fox on charcoal"}' \
  http://127.0.0.1:8787/v1/images
```

Open `http://127.0.0.1:8787/dashboard` and enter the same bridge bearer in the
Connection dialog. The token remains in tab-scoped `sessionStorage`.

## Persistent data

| Path | Purpose | Access |
| --- | --- | --- |
| `/codex-home` | Dedicated OAuth state | Read/write, secret |
| `/data/state` | Sessions, presets, and job history | Read/write |
| `/data/artifacts` | Verified generated outputs | Read/write |
| `/workspace` | Optional source and reference images | Read-only |
| `/config/imagegen-bridge.toml` | Versioned service configuration | Read-only |

Use named volumes or explicit host paths for state and artifacts. Stop the
service or take a SQLite-consistent snapshot before backing up `/data/state`.

## Expose it safely

The default loopback bind is the safest profile. If another host must connect:

1. Keep `server.bearer_token_env` configured.
2. Bind only to a trusted private interface.
3. Put public traffic behind a trusted TLS reverse proxy.
4. Do not publish health detail, OAuth state, artifacts, or the workspace.
5. Verify that `/v1/**` returns `401` without the bearer.

Continue with [deployment and operations](deployment.md) for backups, upgrades,
timeouts, and troubleshooting.
