<div align="center">
  <img src="https://github.com/Crimsab/imagegen-bridge/raw/refs/heads/main/crates/server/dashboard/icon.png" alt="Imagegen Bridge logo" width="144">

# Imagegen Bridge

**Use image generation from your Codex subscription—no OpenAI API key required.**

[![Latest release](https://img.shields.io/github/v/release/Crimsab/imagegen-bridge?display_name=tag&sort=semver&label=release)](https://github.com/Crimsab/imagegen-bridge/releases/latest)
[![CI](https://img.shields.io/github/actions/workflow/status/Crimsab/imagegen-bridge/ci.yml?branch=main&label=CI)](https://github.com/Crimsab/imagegen-bridge/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/imagegen-bridge-cli?label=crates.io)](https://crates.io/crates/imagegen-bridge-cli)
[![PyPI](https://img.shields.io/pypi/v/imagegen-bridge?label=PyPI)](https://pypi.org/project/imagegen-bridge/)
[![npm](https://img.shields.io/npm/v/imagegen-bridge?label=npm)](https://www.npmjs.com/package/imagegen-bridge)
[![Container: GHCR](https://img.shields.io/badge/container-GHCR-blue?logo=github)](https://github.com/Crimsab/imagegen-bridge/pkgs/container/imagegen-bridge)
[![License: MIT](https://img.shields.io/github/license/Crimsab/imagegen-bridge)](LICENSE)

[Documentation](https://crimsab.github.io/imagegen-bridge/) · [Quick start](#quick-start) · [Commands](#main-commands) · [Examples](#common-workflows) · [SDKs and skill](#use-from-code-or-agents) · [Dashboard and API](#dashboard-and-api)

</div>

![Imagegen Bridge connects Codex OAuth to a CLI, API, dashboard, and typed SDKs](https://github.com/Crimsab/imagegen-bridge/raw/refs/heads/main/docs/assets/hero.png)

Imagegen Bridge connects to the Codex OAuth session from your own subscription
and exposes its image generation through a local CLI, dashboard, and HTTP API.
You keep using your existing Codex login instead of configuring or paying for a
separate OpenAI API key.

## Quick start

You need Rust 1.94, a working `codex` executable, and an existing Codex login.

```sh
cargo install imagegen-bridge-cli
codex login
imagegen-bridge setup
imagegen-bridge doctor
imagegen-bridge generate \
  "A red paper fox on a charcoal background" \
  --output first-image.png \
  --preview
```

Open the local generation dashboard with:

```sh
imagegen-bridge dashboard
```

`setup` previews every change before applying it. Neither `setup` nor `doctor`
spends an image generation unless you explicitly request and confirm a live
probe.

## Main commands

| Command | Purpose |
| --- | --- |
| `imagegen-bridge setup` | Configure the bridge for the current Codex login |
| `imagegen-bridge doctor` | Check OAuth, configuration, storage, and providers |
| `imagegen-bridge generate` | Generate one or more images |
| `imagegen-bridge edit` | Edit an image or use reference images |
| `imagegen-bridge dashboard` | Open or start the embedded dashboard |
| `imagegen-bridge serve` | Run the HTTP API explicitly |
| `imagegen-bridge gateway` | Hold and route requests across single-active blue/green handoffs |
| `imagegen-bridge preset` | Save and reuse complete request settings |
| `imagegen-bridge providers` | Inspect providers, models, and capabilities |
| `imagegen-bridge background remove` | Remove a flat background locally |
| `imagegen-bridge update` | Check, install, or roll back verified releases |

Run `imagegen-bridge <command> --help` for every option. The complete CLI
reference is in [docs/cli.md](docs/cli.md).

The CLI checks for a new GitHub Release at most once per day after successful
interactive commands. It never checks during server, dashboard, JSON, plain, or
quiet execution and never sends telemetry. Set
`IMAGEGEN_BRIDGE_NO_UPDATE_CHECK=1` to disable the passive check completely.

The runtime includes per-provider circuit breakers, W3C trace correlation,
bounded duration metrics, deterministic capacity/failure testing, and an
active/passive updater that never runs two OAuth-backed slots concurrently.
See [the resilience model](docs/resilience.md) and
[capacity guide](docs/capacity.md).

## Common workflows

### Generate and save an image

```sh
imagegen-bridge generate \
  "A studio portrait with deep red hair" \
  --size 1024x1536 \
  --quality high \
  --format png \
  --output portraits/red-hair.png \
  --metadata sidecar \
  --preview
```

Use `--output-dir` for generated filenames, `--collision suffix` to preserve an
existing file, `--open` for the system image viewer, and `--metadata embedded`
to store generation metadata inside the image. Metadata is disabled by default.

### Generate multiple images

```sh
imagegen-bridge generate \
  "Four variations of a stone bridge in fog" \
  --count 4 \
  --batch-execution parallel \
  --output-dir batches/bridges
```

If a transport supports only one image per request, the bridge fans out
independent provider calls and preserves the requested order. The default
`max_parallel_outputs = "auto"` starts every requested output concurrently;
set a positive integer when you deliberately want a cap.

### Edit with references

```sh
imagegen-bridge edit \
  "Change the jacket to dark blue" \
  --image ./portrait.png \
  --reference ./palette.png \
  --response-format artifact
```

### Create transparent output

```sh
imagegen-bridge generate \
  "A small red fox mascot, full body" \
  --background transparent \
  --transparency auto \
  --format png \
  --output mascots/fox.png
```

`auto` uses native alpha when the provider supports it and otherwise applies the
local chroma-key pipeline. Existing keyed images can be processed without a
provider call:

```sh
imagegen-bridge background remove keyed-input.png \
  --output transparent.png \
  --key auto
```

### Reuse settings and threads

```sh
imagegen-bridge preset create portrait-high --from request.json
imagegen-bridge generate "A red-haired woman" --preset portrait-high

imagegen-bridge generate \
  "Create the first character sheet" \
  --session-key character-design
imagegen-bridge generate \
  "Keep the character and show a side view" \
  --session-key character-design
```

Presets never retain image bytes, masks, reference images, or idempotency keys.

## Use from code or agents

| Surface | Install or location |
| --- | --- |
| Rust SDK | `cargo add imagegen-bridge` |
| Python SDK | `uv add imagegen-bridge` |
| TypeScript SDK | `bun add imagegen-bridge` |
| OpenAPI and JSON Schema | `schemas/` |
| Agent skill | `skills/generate-images-with-bridge/` |

Install the agent skill with:

```sh
npx skills add Crimsab/imagegen-bridge \
  --skill generate-images-with-bridge
```

The SDKs and skill discover the same live provider capabilities as the CLI.
See [docs/sdks.md](docs/sdks.md) for examples.

## Dashboard and API

`imagegen-bridge dashboard` opens the embedded UI and starts the local service
when needed. The dashboard supports generation, edits, reference images,
advanced controls, presets, asynchronous jobs, history, previews, metadata,
and provider diagnostics. It is embedded in the Rust binary: no Node runtime
or separate web server is required.

To run it manually:

```sh
imagegen-bridge serve --bind 127.0.0.1:8787
```

Open `http://127.0.0.1:8787/dashboard`, or send a native request:

```sh
curl --fail --silent --show-error \
  -H 'Content-Type: application/json' \
  -d '{"operation":"generate","prompt":"A small stone bridge in fog"}' \
  http://127.0.0.1:8787/v1/images
```

OpenAI-familiar generation and multipart edit routes are also available at
`/v1/images/generations` and `/v1/images/edits`. Durable jobs use `/v1/jobs`
and persist artifact-backed results across restarts. See
[docs/api.md](docs/api.md) for authentication, streaming, presets, job history,
diagnostics, metrics, errors, and the full route contract.

## Providers and models

| Transport | Status | Use it for | Constraint |
| --- | --- | --- | --- |
| `codex-responses` | Default, first class | Built-in Codex image generation, explicit models, and image controls | Uses the Codex Responses backend and Codex/ChatGPT OAuth |
| `codex-app-server` | Compatibility transport | Codex lifecycle, edits, references, and reusable threads | Its turn may complete without emitting an image item; do not use it as an automatic production fallback |

`codex-responses` is the built-in Codex path and can route `gpt-image-2`,
`gpt-image-1.5`, `gpt-image-1`, and `gpt-image-1-mini`. It authenticates with
the existing Codex/ChatGPT OAuth session and the Codex Responses backend. It
does **not** read `OPENAI_API_KEY` and is separate from the official OpenAI
Platform API-key provider. Inspect its live capabilities with:

```sh
imagegen-bridge providers capabilities \
  --provider codex-responses \
  --json

imagegen-bridge generate \
  --model gpt-image-2 \
  "A translucent red glass sculpture"
```

Requests are checked against the selected provider before generation. Strict
mode rejects unsupported combinations; `--compatibility normalize` applies
only changes reported back in the response. Masks are currently unsupported by
both Codex transports and return the stable Codex detail code
`CODEX_RASTER_MASK_UNSUPPORTED`. Source-only edits are contextual edits: the
bridge attaches verified images to the current Codex turn. See the
[Codex contextual-edit integration contract](docs/api.md#codex-contextual-edit-integration).

`codex-app-server` remains available as a compatibility transport. Its upstream turn may
occasionally finish without emitting an image item; the bridge treats that as
an error and records content-safe structured counts of observed item types and
statuses for diagnosis. Prefer `codex-responses` for production routing rather
than automatically falling back to app-server. An API-key-backed official OpenAI provider is reserved
in configuration but is not implemented and never shares Codex OAuth handling.

`codex-responses` retries once by default only when the upstream explicitly
reports a transient failure before returning an image, including a failed
`image_generation_call` or a completed response with no image item. Transport
timeouts, cancellations, safety or
permission failures, malformed output, and every unknown outcome are never
retried automatically. Configure the bounded policy with
`max_transient_attempts` and `transient_retry_backoff_ms`.

## Advanced controls

The native contract supports, where the provider allows them:

- size, aspect ratio, resolution, quality, format, compression, and background;
- multiple outputs with sequential, automatic full-fan-out, or explicitly capped parallel execution;
- negative-prompt, moderation, revised-prompt, and partial-image policies;
- input fidelity, reference images, edits, and reusable sessions;
- native or emulated transparency with alpha validation and despill;
- atomic output files, sidecar or embedded metadata, timings, and checksums;
- ordered provider/model fallbacks with per-attempt traces;
- strict, normalized, fail-fast, and best-effort request behavior.

Provider fallback is explicit and never reroutes safety refusals, cancellation,
permission failures, or operations with an unknown upstream outcome:

```sh
imagegen-bridge generate "A red paper fox" \
  --provider codex-responses \
  --fallback codex-app-server:gpt-image-2 \
  --fallback-policy on_unavailable
```

## Docker

Released users can run the published GHCR package without cloning or compiling
the repository. See the [container package](https://github.com/Crimsab/imagegen-bridge/pkgs/container/imagegen-bridge)
and the complete [Docker quickstart](docs/docker-quickstart.md).

To build the container from source instead:

```sh
git clone https://github.com/Crimsab/imagegen-bridge.git
cd imagegen-bridge
export IMAGEGEN_BRIDGE_BEARER_TOKEN="$(openssl rand -hex 32)"
export IMAGEGEN_BRIDGE_CODEX_HOME="$HOME/.codex"
docker compose up --build -d
```

The container runs as UID/GID 10001, supports a read-only root filesystem, and
keeps OAuth state, SQLite data, artifacts, and reference inputs in separate
mounts. Tagged releases publish Linux AMD64 and ARM64 images at
`ghcr.io/crimsab/imagegen-bridge`. Read
[docs/deployment.md](docs/deployment.md) before exposing the API or mounting
Codex credentials.

## Configuration

Configuration precedence is:

```text
defaults < TOML file < IMAGEGEN_BRIDGE__* environment < --set/--unset
```

Start with `imagegen-bridge setup`, or use
[config.example.toml](config.example.toml) for a hand-managed deployment.
Unknown keys fail validation. `config show` and `config origins` report the
effective configuration without resolving credential values.

## Documentation

- [Documentation site](https://crimsab.github.io/imagegen-bridge/)
- [CLI reference](docs/cli.md)
- [HTTP API contract](docs/api.md)
- [SDK guide](docs/sdks.md)
- [Deployment and operations](docs/deployment.md)
- [Testing matrix](docs/testing.md)
- [Release process](docs/releasing.md)
- [Contributing](CONTRIBUTING.md)

## Testing

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
```

Offline tests use fake Codex processes, mock HTTP services, fixture images, and
the production dashboard. Live OAuth tests are individually gated because they
can consume image-generation allowance. See [docs/testing.md](docs/testing.md).

## Security and upstream status

The default Responses adapter is live-tested against the Codex backend. Its
wire protocol is an implementation detail of Codex OAuth and may evolve, so
releases keep regression fixtures and controlled live gates. Imagegen Bridge
does not disable or bypass upstream safety checks and does not automatically
retry an unchanged safety refusal.

Do not commit `auth.json`, mount an entire home directory into the container, or
bind an unauthenticated bridge to a public interface.

## License

Licensed under the [MIT License](LICENSE).
