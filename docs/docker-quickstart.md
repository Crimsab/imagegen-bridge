# Docker quickstart

The recommended Docker path pulls the published package from GHCR. It does not
clone the repository and does not compile Rust locally. The container is
available for Linux AMD64 and ARM64.

## Choose the right Docker path

| Goal | Use | Repository clone |
| --- | --- | --- |
| Run the released service | `compose.package.yaml` and the GHCR image | No |
| Modify or audit the image | `compose.yaml` and the included `Dockerfile` | Yes |
| Develop the Rust project | Source checkout and the normal test workflow | Yes |

The package Compose file runs the service as a non-root process with a
read-only root filesystem, dropped Linux capabilities, bounded temporary
storage, persistent named volumes, and a loopback host bind. It requires Docker
Compose v2. The safe container settings are passed through the bridge's native
configuration overrides, so no second configuration file is required.

## Run the published package

Create an empty deployment directory and download the standalone Compose file:

```sh
mkdir imagegen-bridge && cd imagegen-bridge
curl --fail --location --remote-name \
  https://raw.githubusercontent.com/Crimsab/imagegen-bridge/main/compose.package.yaml
```

Create a dedicated Codex home. Codex may rotate its OAuth state, so this mount
must remain writable by container UID `10001`.

```sh
install -d -m 0700 ./codex-home
cp "$HOME/.codex/auth.json" ./codex-home/auth.json
chown -R 10001:10001 ./codex-home
```

Never commit or share this directory. It contains the Codex OAuth credential,
which is separate from the bridge bearer token.

Start the released image:

```sh
export IMAGEGEN_BRIDGE_BEARER_TOKEN="$(openssl rand -hex 32)"
export IMAGEGEN_BRIDGE_CODEX_HOME="$PWD/codex-home"
docker compose -f compose.package.yaml pull
docker compose -f compose.package.yaml up -d
docker compose -f compose.package.yaml ps
```

The file pins `ghcr.io/crimsab/imagegen-bridge:0.1.1`. Set
`IMAGEGEN_BRIDGE_IMAGE` to another released tag when you intentionally upgrade.
Using a versioned tag keeps restarts reproducible; `latest` is available but is
not the recommended production pin.

## Verify the service

Check the public, detail-free health endpoints:

```sh
curl --fail http://127.0.0.1:8787/health/live
curl --fail http://127.0.0.1:8787/health/ready
```

Make the first authenticated request:

```sh
curl --fail --silent --show-error \
  -H "Authorization: Bearer $IMAGEGEN_BRIDGE_BEARER_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"version":"1","operation":"generate","prompt":"A paper fox on charcoal"}' \
  http://127.0.0.1:8787/v1/images
```

Open `http://127.0.0.1:8787/dashboard` and enter the same bridge bearer in the
Connection dialog. The token remains in tab-scoped `sessionStorage`.

## Build from source instead

Clone only when you want the repository's Dockerfile to compile the Rust binary
and assemble a local image:

```sh
git clone https://github.com/Crimsab/imagegen-bridge.git
cd imagegen-bridge
install -d -m 0700 ./deploy/codex-home
cp "$HOME/.codex/auth.json" ./deploy/codex-home/auth.json
chown -R 10001:10001 ./deploy/codex-home
export IMAGEGEN_BRIDGE_BEARER_TOKEN="$(openssl rand -hex 32)"
export IMAGEGEN_BRIDGE_CODEX_HOME="$PWD/deploy/codex-home"
docker compose up --build -d
```

In this path, `compose.yaml` uses the included multi-stage `Dockerfile`; the
clone is build input, not an installation requirement for released users.

## Persistent data

| Path | Purpose | Access |
| --- | --- | --- |
| `/codex-home` | Dedicated OAuth state | Read/write, secret |
| `/data/state` | Sessions, presets, and job history | Read/write |
| `/data/artifacts` | Verified generated outputs | Read/write |
| `/workspace` | Optional source and reference images | Read-only |
| Command configuration | Versioned safe service defaults | Container arguments |

The package Compose file uses named volumes for state and artifacts. Stop the
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
