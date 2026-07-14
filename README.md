<div align="center">
  <img src="https://github.com/Crimsab/imagegen-bridge/raw/refs/heads/main/crates/server/dashboard/icon.png" alt="Imagegen Bridge logo" width="144">

# Imagegen Bridge

**Use image generation from your Codex subscription—no OpenAI API key required.**

[![Latest release](https://img.shields.io/github/v/release/Crimsab/imagegen-bridge?display_name=tag&sort=semver&label=release)](https://github.com/Crimsab/imagegen-bridge/releases/latest)
[![Release downloads](https://img.shields.io/github/downloads/Crimsab/imagegen-bridge/total?label=downloads)](https://github.com/Crimsab/imagegen-bridge/releases)
[![License: MIT](https://img.shields.io/github/license/Crimsab/imagegen-bridge)](LICENSE)

[Quick start](#quick-start) · [Commands](#main-commands) · [Examples](#common-workflows) · [SDKs and skill](#use-from-code-or-agents) · [Dashboard and API](#dashboard-and-api) · [Documentation](#documentation)

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
| `imagegen-bridge preset` | Save and reuse complete request settings |
| `imagegen-bridge providers` | Inspect providers, models, and capabilities |
| `imagegen-bridge background remove` | Remove a flat background locally |

Run `imagegen-bridge <command> --help` for every option. The complete CLI
reference is in [docs/cli.md](docs/cli.md).

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

If a transport supports only one image per request, the bridge fans out bounded
provider calls and preserves the requested order.

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
| `codex-app-server` | Default | Supported Codex lifecycle, edits, references, and reusable threads | The image tool exposes a small automatic parameter set |
| `codex-responses` | Opt-in experimental | Explicit models and additional image controls | Uses a private upstream protocol that may change |

The experimental Responses transport can route `gpt-image-2`,
`gpt-image-1.5`, `gpt-image-1`, and `gpt-image-1-mini`. Enable it in the
configuration, inspect its live capabilities, and select it explicitly:

```sh
imagegen-bridge providers capabilities \
  --provider codex-responses \
  --json

imagegen-bridge generate \
  --provider codex-responses \
  --model gpt-image-2 \
  "A translucent red glass sculpture"
```

Requests are checked against the selected provider before generation. Strict
mode rejects unsupported combinations; `--compatibility normalize` applies
only changes reported back in the response. Masks are currently unsupported by
both Codex transports.

An API-key-backed official OpenAI provider is reserved in configuration but is
not implemented. The current project focuses on Codex OAuth.

## Advanced controls

The native contract supports, where the provider allows them:

- size, aspect ratio, resolution, quality, format, compression, and background;
- multiple outputs with sequential or bounded parallel execution;
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
  --provider codex-app-server \
  --fallback codex-responses:gpt-image-2 \
  --fallback-policy on_unavailable
```

## Docker

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

The Responses adapter is live-tested, but its private upstream protocol may
change. App-server remains the default. Imagegen Bridge does not disable or
bypass upstream safety checks and does not automatically retry an unchanged
safety refusal.

Do not commit `auth.json`, mount an entire home directory into the container, or
bind an unauthenticated bridge to a public interface.

## License

Licensed under the [MIT License](LICENSE).
