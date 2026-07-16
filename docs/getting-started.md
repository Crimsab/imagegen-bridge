# Choose your path

Imagegen Bridge exposes the same generation contract through a CLI, a private
HTTP service, a dashboard, and typed SDKs. Start with the surface that matches
how you plan to use it.

## Local command line

Choose the CLI when one person is generating or editing images on one machine.
It guides the initial setup, verifies the existing Codex login, and writes
validated image files directly to disk.

```sh
cargo install imagegen-bridge-cli
codex login
imagegen-bridge setup
imagegen-bridge doctor
imagegen-bridge generate "A paper fox on a charcoal background" \
  --output first-image.png
```

Continue with the [command-line guide](cli.md).

## Private service

Choose Docker when applications, agents, or several machines need one stable
endpoint. The service adds bearer authentication, streaming, durable jobs,
artifacts, metrics, and the embedded dashboard.

```sh
git clone https://github.com/Crimsab/imagegen-bridge.git
cd imagegen-bridge
export IMAGEGEN_BRIDGE_BEARER_TOKEN="$(openssl rand -hex 32)"
export IMAGEGEN_BRIDGE_CODEX_HOME="$HOME/.codex"
docker compose up --build -d
```

Read the [Docker quickstart](docker-quickstart.md) before binding outside host
loopback.

## Application integration

Use an SDK when your application needs typed requests, streaming events,
deadlines, jobs, or structured errors.

=== "Python"

    ```sh
    uv add imagegen-bridge
    ```

=== "TypeScript"

    ```sh
    bun add imagegen-bridge
    ```

=== "Rust"

    ```sh
    cargo add imagegen-bridge
    ```

Continue with the [SDK guide](sdks.md).

## Before the first live generation

1. Confirm that `codex login` succeeds for the account you intend to use.
2. Run `imagegen-bridge doctor` without a live probe.
3. Inspect provider capabilities rather than assuming model features.
4. Keep the HTTP listener on loopback unless bearer authentication is enabled.
5. Treat OAuth state and the bridge bearer as separate credentials.

The non-generating checks do not consume image allowance. A live probe does,
and the CLI asks for confirmation before running one.
