# Docker quickstart

This guide installs the released Imagegen Bridge container on one machine and
exposes its dashboard and API at `http://127.0.0.1:8787`. It does not clone the
repository and does not compile Rust locally.

- **Container package:** [GitHub Container Registry](https://github.com/Crimsab/imagegen-bridge/pkgs/container/imagegen-bridge)
- **Image:** `ghcr.io/crimsab/imagegen-bridge:0.2.0`
- **Platforms:** Linux AMD64 and ARM64
- **Registry login:** not required; the package is public

The commands below target Linux, macOS with Docker Desktop, and WSL2. Run them
from a POSIX shell.

## Before you start

You need Docker with the Compose v2 plugin, `curl`, `openssl`, and a working
Codex login on the host.

```sh
docker --version
docker compose version
codex login
test -s "${CODEX_HOME:-$HOME/.codex}/auth.json"
```

All four commands must succeed. `codex login` creates the OAuth state used by
the bridge; it does not create the separate bearer token that protects the
bridge API.

!!! info "What Docker will create"

    One non-root container, two named volumes for state and generated
    artifacts, one private `codex-home` directory, and one local `.env` file.
    Only port `127.0.0.1:8787` is published by default.

## 1. Download the deployment file

Create a dedicated directory and download the standalone Compose file:

```sh
mkdir imagegen-bridge
cd imagegen-bridge
curl --fail --location --remote-name \
  https://raw.githubusercontent.com/Crimsab/imagegen-bridge/main/compose.package.yaml
```

You should now have only `compose.package.yaml`. It describes the ports,
volumes, security restrictions, health check, and released image to run.

## 2. Copy the Codex login

Keep a dedicated copy of the Codex OAuth state instead of mounting your entire
home directory:

```sh
CODEX_AUTH="${CODEX_HOME:-$HOME/.codex}/auth.json"
install -d -m 0700 ./codex-home
install -m 0600 "$CODEX_AUTH" ./codex-home/auth.json
```

On Linux and WSL2, make the directory writable by the non-root container user:

```sh
sudo chown -R 10001:10001 ./codex-home
```

Docker Desktop on macOS normally handles bind-mount permissions without the
`chown` command. Never commit, upload, or share `codex-home`; it contains your
Codex OAuth credential. The container may update this dedicated copy when OAuth
state rotates.

## 3. Create and save the bridge bearer

Generate the API bearer once and save it in a local `.env` file so restarts do
not depend on an old terminal session:

```sh
umask 077
printf 'IMAGEGEN_BRIDGE_BEARER_TOKEN=%s\n' "$(openssl rand -hex 32)" > .env
chmod 0600 .env
```

The Compose file reads `.env` automatically. This bearer protects the bridge
API and dashboard; it is not your Codex credential. To display it when entering
it in the dashboard:

```sh
sed -n 's/^IMAGEGEN_BRIDGE_BEARER_TOKEN=//p' .env
```

Treat the output like a password. Do not paste it into issues, logs, or chat.

## 4. Pull and start the package

Pull the released image explicitly:

```sh
docker pull ghcr.io/crimsab/imagegen-bridge:0.2.0
```

The same image is listed in `compose.package.yaml`. Compose then starts it with
the required mounts and security settings:

```sh
docker compose -f compose.package.yaml up -d
docker compose -f compose.package.yaml ps
```

Wait until `ps` reports the service as `healthy`. `docker pull` by itself only
downloads the image; it does not create or start a container. Compose is used
instead of a long `docker run` command so the configuration remains repeatable.

## 5. Open and verify the service

Open [http://127.0.0.1:8787/dashboard](http://127.0.0.1:8787/dashboard), choose
the Connection dialog, and enter the bearer stored in `.env`.

Check the two detail-free health endpoints from the terminal:

```sh
curl --fail http://127.0.0.1:8787/health/live
curl --fail http://127.0.0.1:8787/health/ready
```

- `live` confirms that the HTTP process is running.
- `ready` confirms that the configured provider can accept work.

If `live` succeeds but `ready` fails, inspect bounded logs and verify the copied
Codex login:

```sh
docker compose -f compose.package.yaml logs --tail 100 imagegen-bridge
ls -l ./codex-home/auth.json
```

## 6. Make an optional generation request

This request performs a real generation and consumes image allowance. Load the
saved bearer only for the current shell, then call the API:

```sh
export IMAGEGEN_BRIDGE_BEARER_TOKEN="$(
  sed -n 's/^IMAGEGEN_BRIDGE_BEARER_TOKEN=//p' .env
)"
curl --fail --silent --show-error \
  -H "Authorization: Bearer $IMAGEGEN_BRIDGE_BEARER_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"version":"1","operation":"generate","prompt":"A paper fox on charcoal"}' \
  http://127.0.0.1:8787/v1/images
unset IMAGEGEN_BRIDGE_BEARER_TOKEN
```

## Everyday commands

Run these commands from the directory containing `compose.package.yaml`:

```sh
# Status
docker compose -f compose.package.yaml ps

# Recent logs
docker compose -f compose.package.yaml logs --tail 100 imagegen-bridge

# Stop and preserve state/artifacts
docker compose -f compose.package.yaml down

# Start again
docker compose -f compose.package.yaml up -d
```

Stopping with `down` removes the container and network but preserves the two
named volumes, `.env`, and `codex-home`.

## Upgrade to another released tag

Open the [package versions](https://github.com/Crimsab/imagegen-bridge/pkgs/container/imagegen-bridge)
and choose a released tag. Add or update this line in `.env`:

```dotenv
IMAGEGEN_BRIDGE_IMAGE=ghcr.io/crimsab/imagegen-bridge:0.2.0
```

Replace `0.2.0` with the desired release, then recreate the container:

```sh
docker compose -f compose.package.yaml pull
docker compose -f compose.package.yaml up -d
docker compose -f compose.package.yaml ps
```

Versioned tags make upgrades explicit and rollbacks predictable. The `latest`
tag exists, but the standalone Compose file intentionally defaults to a fixed
release. If a host-installed `imagegen-bridge` CLI is available, the equivalent
guarded workflow is `imagegen-bridge update docker --dry-run`, followed by
`imagegen-bridge update docker --yes`; use `--compose-file` and `--env-file`
when they are not the defaults.

## Change the port

If port `8787` is already in use, add this line to `.env`:

```dotenv
IMAGEGEN_BRIDGE_PORT=8788
```

Recreate the container and use `http://127.0.0.1:8788/dashboard`:

```sh
docker compose -f compose.package.yaml up -d
```

## Uninstall

To remove only the running service while retaining its data, use `down` as
shown above. To permanently remove the container and its state and artifact
volumes:

```sh
docker compose -f compose.package.yaml down --volumes
```

!!! danger "Permanent data deletion"

    `down --volumes` deletes bridge job history, sessions, and generated
    artifacts stored in Docker volumes. It does not delete `.env` or
    `codex-home`; remove those local credential files separately only when you
    are certain they are no longer needed.

## Build from source instead

Clone the repository only when you want the included multi-stage `Dockerfile`
to compile the Rust binary or when you intend to modify the image:

```sh
git clone https://github.com/Crimsab/imagegen-bridge.git
cd imagegen-bridge
install -d -m 0700 ./deploy/codex-home
install -m 0600 "${CODEX_HOME:-$HOME/.codex}/auth.json" \
  ./deploy/codex-home/auth.json
sudo chown -R 10001:10001 ./deploy/codex-home
umask 077
printf 'IMAGEGEN_BRIDGE_BEARER_TOKEN=%s\nIMAGEGEN_BRIDGE_CODEX_HOME=%s\n' \
  "$(openssl rand -hex 32)" "$PWD/deploy/codex-home" > .env
docker compose up --build -d
```

The clone is build input for `Dockerfile`; it is not an installation
requirement for users of the released GHCR package.

## Storage and safe exposure

| Path | Purpose | Access |
| --- | --- | --- |
| `./.env` | Bridge bearer and deployment overrides | Local secret |
| `./codex-home` | Dedicated Codex OAuth state | Local secret, read/write |
| `/data/state` | Sessions, presets, and job history | Named volume |
| `/data/artifacts` | Verified generated outputs | Named volume |
| `/workspace` | Optional source and reference images | Container-local by default |

The default loopback bind is the safest profile. Before accepting connections
from another machine, read [deployment and operations](deployment.md). Keep
bearer authentication enabled, bind only to a trusted private interface, and
put public traffic behind a trusted TLS reverse proxy.
