# Imagegen Bridge

Imagegen Bridge exposes image generation from an existing Codex OAuth login as
a command-line tool, a local HTTP service, and typed Rust, Python, and
TypeScript clients. Codex-backed usage does not require an `OPENAI_API_KEY`.

The project has two Codex transports:

- `codex-app-server` uses the supported Codex app-server lifecycle. It handles
  process supervision, reference images, edits, and persistent thread reuse,
  but the current Codex image tool exposes only a small automatic parameter set.
- `codex-responses` sends image-tool requests through the private Codex
  Responses endpoint. It exposes more image controls and model selection, but
  is opt-in and experimental because that upstream protocol can change without
  notice.

The repository is pre-release. Build it from source; no crates, Python wheels,
npm packages, binaries, or container images are published yet.
There is currently no setup wizard or web dashboard; configuration and usage
are through TOML, the CLI, or the HTTP/SDK interfaces.

## What is implemented

- Generation and reference-based editing through Codex OAuth.
- Explicit provider/model capability discovery and request negotiation.
- `gpt-image-2`, `gpt-image-1.5`, `gpt-image-1`, and
  `gpt-image-1-mini` routing on the experimental Responses transport.
- Multiple images, size, quality, PNG/JPEG/WebP, compression, background,
  moderation, negative-prompt policy, revised-prompt policy, bounded
  concurrency, and explicit partial-failure behavior where supported.
- Independent decoding and verification of format, dimensions, byte length,
  and SHA-256 before an output is returned or stored.
- Atomic artifact writes, bounded local/remote inputs, retention cleanup, and
  SSRF controls.
- Isolated, persistent, and explicit-thread sessions for app-server.
- A native JSON API plus OpenAI-familiar generation and multipart edit routes.
- Optional bridge bearer authentication, readiness checks, JSON tracing, and
  Prometheus metrics.
- Rust library facade and typed Python and TypeScript HTTP clients.
- A non-root, read-only-compatible container build with a pinned Codex binary.

The configuration contains a reserved official OpenAI provider section, but an
API-key-backed OpenAI provider is not implemented or registered yet.

## Build and authenticate

Requirements:

- Rust 1.94 or the pinned toolchain from `rust-toolchain.toml`.
- A working `codex` executable.
- An existing Codex OAuth login (`codex login`) for live generation.

```sh
git clone https://github.com/Crimsab/imagegen-bridge.git
cd imagegen-bridge
cargo build --locked --release -p imagegen-bridge-cli
cp config.example.toml imagegen-bridge.toml
./target/release/imagegen-bridge auth-doctor
```

You can also install the local checkout into Cargo's binary directory:

```sh
cargo install --locked --path crates/cli
```

`auth-doctor` checks authentication without generating an image. It does not
perform a paid/live image request.

## CLI usage

Generate an artifact using the default app-server provider:

```sh
imagegen-bridge generate \
  --prompt "A red paper fox on a charcoal background" \
  --response-format artifact \
  --filename-prefix paper-fox
```

Edit an image or add visual references:

```sh
imagegen-bridge edit \
  --prompt "Change the jacket to dark blue" \
  --image ./portrait.png \
  --reference ./palette.png \
  --response-format artifact
```

Use a persistent app-server thread:

```sh
imagegen-bridge generate \
  --prompt "Create the first character sheet" \
  --session-key character-design \
  --response-format artifact

imagegen-bridge generate \
  --prompt "Keep the character and show a side view" \
  --session-key character-design \
  --response-format artifact
```

Inspect the effective provider surface before using advanced flags:

```sh
imagegen-bridge providers list --json
imagegen-bridge providers capabilities --provider codex-app-server --json
imagegen-bridge providers readiness --json
```

Requests default to strict compatibility. Unsupported combinations fail before
generation. `--compatibility normalize` allows only transformations reported in
the response's `normalizations` field. `--dry-run` validates and prints the
request without starting Codex or opening output storage. The current Codex
transports reject `--user` explicitly because upstream attribution support has
not been proven; they never silently discard it.

For the Responses adapter, `n > 1` uses at most
`providers.codex_responses.max_parallel_outputs` simultaneous upstream calls
(default `2`, maximum `4`). `failure_policy=fail_fast` cancels outstanding work
on the first failure. `best_effort` returns successful images in requested-index
order plus structured `failures`; every success and failure includes its output
index and per-item generation time.

To use the experimental Responses provider, set
`providers.codex_responses.enabled = true` in the TOML configuration, then
select it explicitly:

```sh
imagegen-bridge generate \
  --provider codex-responses \
  --model gpt-image-1.5 \
  --prompt "A translucent red glass sculpture" \
  --background transparent \
  --format png \
  --response-format artifact
```

The full CLI reference, output modes, exit codes, configuration overrides,
session commands, schema commands, completions, and man-page generation are in
[docs/cli.md](docs/cli.md).

## HTTP service

Start the server on loopback:

```sh
imagegen-bridge serve --bind 127.0.0.1:8787
```

Minimal native request:

```sh
curl --fail --silent --show-error \
  -H 'Content-Type: application/json' \
  -d '{"operation":"generate","prompt":"A small stone bridge in fog"}' \
  http://127.0.0.1:8787/v1/images
```

OpenAI-familiar generation request:

```sh
curl --fail --silent --show-error \
  -H 'Content-Type: application/json' \
  -d '{"prompt":"A small stone bridge in fog","response_format":"b64_json"}' \
  http://127.0.0.1:8787/v1/images/generations
```

Important routes:

| Route | Purpose |
| --- | --- |
| `POST /v1/images` | Lossless native generation/edit contract |
| `POST /v1/images/generations` | OpenAI-familiar JSON generation |
| `POST /v1/images/edits` | OpenAI-familiar multipart editing |
| `POST /v1/images/stream` | Bounded SSE progress/partial/completion stream |
| `GET /v1/providers` | Provider inventory |
| `GET /v1/providers/{provider}/capabilities` | Model-aware capabilities |
| `GET /v1/sessions/{key}` | Persistent session lookup |
| `GET /health/live` | Process liveness |
| `GET /health/ready` | Provider readiness |
| `GET /v1/openapi.json` | Generated OpenAPI 3.1 document |

Configure `server.bearer_token_env` before exposing the service outside a
trusted loopback environment. The bridge token protects its own API and is
separate from the upstream Codex OAuth credential. See
[docs/api.md](docs/api.md).

## Libraries and integrations

| Integration | Location | Runtime |
| --- | --- | --- |
| Rust facade | `crates/imagegen-bridge` | In-process provider/runtime API |
| Python SDK | `sdks/python` | Sync/async HTTPX client, typed models, SSE |
| TypeScript SDK | `sdks/typescript` | Dependency-free Fetch client for Bun/Node |
| OpenAPI/JSON Schema | `schemas/` | Generated wire contracts |
| Container | `Dockerfile`, `compose.yaml` | Bridge and pinned Codex CLI |

Examples and package build commands are in [docs/sdks.md](docs/sdks.md). The
OpenAI-familiar routes make migration from a subset of Images API calls small,
while the native route preserves sessions, normalizations, timings, warnings,
and verified artifact metadata.

## Configuration

Configuration is merged in this order:

```text
defaults < TOML file < IMAGEGEN_BRIDGE__* environment < --set/--unset
```

Nested environment keys use double underscores, for example:

```sh
export IMAGEGEN_BRIDGE__RUNTIME__GLOBAL__MAX_CONCURRENT=8
```

Unknown configuration keys fail validation. `config show` and `config origins`
report effective settings and provenance without resolving credential values.
Start from [config.example.toml](config.example.toml).

## Container

The image runs as UID/GID 10001, uses a read-only-compatible root filesystem,
and keeps OAuth state, SQLite state, artifacts, and reference inputs in separate
mounts. The Compose example binds the API to `127.0.0.1` by default.

```sh
export IMAGEGEN_BRIDGE_BEARER_TOKEN="$(openssl rand -hex 32)"
export IMAGEGEN_BRIDGE_CODEX_HOME="$PWD/deploy/codex-home"
docker compose up --build -d
```

Read [docs/deployment.md](docs/deployment.md) before mounting Codex credentials
or exposing the API on a network.

## Testing

The ordinary test suite uses fake Codex processes, mock HTTP servers, golden SSE
fixtures, and independently decoded image fixtures. It does not generate paid
images.

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
```

Live Codex tests are ignored unless their explicit environment gates are set:
`IMAGEGEN_BRIDGE_LIVE_CODEX=1`, `IMAGEGEN_BRIDGE_LIVE_CODEX_RESPONSES=1`, or
`IMAGEGEN_BRIDGE_LIVE_BOOTSTRAP=1`.

## Security and upstream status

The direct Responses adapter uses a private ChatGPT/Codex endpoint. It may stop
working when the upstream protocol changes and is deliberately marked
experimental in discovery responses. The app-server adapter is the default.

Do not commit `auth.json`, mount an entire home directory into the container, or
bind an unauthenticated bridge to a public interface. Imagegen Bridge does not
disable or bypass upstream safety checks. Safety refusals are returned as
`safety_rejected` / `moderation_blocked` with a stable recovery hint to revise
the prompt or input images; the unchanged request is not retried automatically.

## License

Licensed under either the Apache License 2.0 or the MIT License, at your option.
See [LICENSE-APACHE](LICENSE-APACHE) and [LICENSE-MIT](LICENSE-MIT).
