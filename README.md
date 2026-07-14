<div align="center">
  <img src="crates/server/dashboard/icon.png" alt="Imagegen Bridge logo" width="144">

# Imagegen Bridge

**Use an existing Codex OAuth login for image generation through a CLI, local API, dashboard, or typed SDK.**

[![CI](https://github.com/Crimsab/imagegen-bridge/actions/workflows/ci.yml/badge.svg)](https://github.com/Crimsab/imagegen-bridge/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-2f855a.svg)](LICENSE)
[![MSRV: 1.94](https://img.shields.io/badge/MSRV-1.94-93450a.svg)](rust-toolchain.toml)

[Install](#install) · [Quick start](#quick-start) · [CLI](docs/cli.md) · [HTTP API](docs/api.md) · [SDKs](docs/sdks.md) · [Deployment](docs/deployment.md)

</div>

<!-- Hero artwork slot: add docs/assets/hero.webp here when the final artwork is ready. -->

Imagegen Bridge turns an existing Codex OAuth login into a command-line tool,
local HTTP service, embedded dashboard, and typed Rust, Python, and TypeScript
clients. Codex-backed usage does not require an `OPENAI_API_KEY`.

### Choose a Codex transport

| Transport | Status | Best for | Main constraint |
| --- | --- | --- | --- |
| `codex-app-server` | Default | Supported Codex lifecycle, reference images, edits, and reusable threads | The current image tool exposes a small automatic parameter set |
| `codex-responses` | Opt-in, live-tested experimental | Model selection and more image controls | Uses a private upstream protocol that may change without notice |

## Capabilities

- Generation and reference-based editing through Codex OAuth.
- Explicit provider/model capability discovery and request negotiation.
- `gpt-image-2`, `gpt-image-1.5`, `gpt-image-1`, and
  `gpt-image-1-mini` routing on the experimental Responses transport.
- Multiple images, size, quality, PNG/JPEG/WebP, compression, background,
  moderation, negative-prompt policy, revised-prompt policy, bounded
  concurrency, and explicit partial-failure behavior where supported.
- Transparent output through provider-native alpha or a self-contained
  GPT Image 2 chroma-key pipeline with prompt-aware key selection, border
  sampling, soft matte, despill, and alpha validation.
- Ordered provider/model fallback routes with conservative unknown-outcome and
  safety guards plus an attempt trace in responses and sidecar metadata.
- Capability-checked edit action and input fidelity. `gpt-image-2` image inputs
  are always high fidelity; older Responses-routed image models accept explicit
  `low`/`high`. Masks remain explicitly unsupported by both Codex transports.
- Independent decoding and verification of format, dimensions, byte length,
  and SHA-256 before an output is returned or stored.
- Atomic artifact writes, bounded local/remote inputs, retention cleanup,
  conservative ownership audit/repair, and SSRF controls.
- Isolated, persistent, and explicit-thread sessions for app-server.
- A native JSON API plus OpenAI-familiar generation and multipart edit routes.
- Durable asynchronous jobs with a bounded SQLite queue, restart-safe history,
  cancellation, progress snapshots, artifact-only results, and cursor pagination.
- A dependency-free embedded dashboard for generation, edits, reference images,
  advanced controls, authenticated previews, metadata, and history management.
- Optional bridge bearer authentication, readiness checks, JSON tracing, and
  Prometheus metrics.
- Rust library facade and typed Python and TypeScript HTTP clients.
- A non-root, read-only-compatible container build with a pinned Codex binary.

The configuration reserves an official OpenAI provider section, but an
API-key-backed OpenAI provider is not implemented or registered.

## Install

Version tags create platform CLI archives with checksums and publish the Rust,
Python, TypeScript, and container packages. The package names are consistent
across registries:

| Surface | Install command |
| --- | --- |
| CLI | `cargo install imagegen-bridge-cli` |
| Rust SDK | `cargo add imagegen-bridge` |
| Python SDK | `uv add imagegen-bridge` |
| TypeScript SDK | `bun add imagegen-bridge` |
| Container | `docker pull ghcr.io/crimsab/imagegen-bridge:latest` |
| Agent skill | `npx skills add Crimsab/imagegen-bridge --skill generate-images-with-bridge` |

Binary archives are attached to [GitHub Releases](https://github.com/Crimsab/imagegen-bridge/releases).
Registry setup and the tag-driven publication process are documented in
[docs/releasing.md](docs/releasing.md).

## Quick start

Requirements:

- Rust 1.94 or the pinned toolchain from `rust-toolchain.toml`.
- A working `codex` executable.
- An existing Codex OAuth login (`codex login`) for live generation.

```sh
cargo install imagegen-bridge-cli
imagegen-bridge setup
imagegen-bridge doctor
imagegen-bridge generate \
  "A red paper fox on a charcoal background" \
  --output first-image.png \
  --preview
```

You can also install the local checkout into Cargo's binary directory:

```sh
cargo install --locked --path crates/cli
```

The final command performs a live Codex generation, writes `first-image.png`
below the configured artifact root, and previews it in a supported terminal.
`setup` detects Codex and ChatGPT OAuth, previews every filesystem change,
writes a user configuration atomically, creates private state and artifact
directories, and applies the session and job SQLite schemas idempotently. It
never generates an image unless `--live-probe` is explicitly requested and
confirmed. Use
`setup --dry-run --json` to inspect the plan or `setup --yes --non-interactive`
for automation. `doctor` checks the executable/version, configuration, OAuth,
permissions, database schema, listener availability, provider readiness, and
dynamic capabilities. `doctor --live-probe` adds one confirmed paid generation.

### Docker Compose

The repository also includes a hardened non-root Compose deployment:

```sh
git clone https://github.com/Crimsab/imagegen-bridge.git
cd imagegen-bridge
export IMAGEGEN_BRIDGE_BEARER_TOKEN="$(openssl rand -hex 32)"
export IMAGEGEN_BRIDGE_CODEX_HOME="$HOME/.codex"
docker compose up --build -d
curl --fail http://127.0.0.1:8787/health/live
```

Read [docs/deployment.md](docs/deployment.md) before exposing the listener or
mounting Codex credentials into a shared environment.

## CLI usage

### Generate and store images

Generate an artifact using the default app-server provider:

```sh
imagegen-bridge generate \
  "A red paper fox on a charcoal background" \
  --output portraits/paper-fox.png \
  --collision suffix \
  --metadata sidecar \
  --preview
```

`--output` selects an exact filename for a single image and automatically uses
artifact delivery. `--output-dir batches/july` keeps generated UUID filenames
inside that directory. Paths are constrained below the configured artifact
root; existing exact names fail atomically unless `--collision suffix` is
selected. The native API and SDKs expose the same controls as
`output.directory`, `output.filename`, and `output.collision`.
`--metadata sidecar` writes an owned JSON record containing prompts,
requested/effective parameters, model/provider, timings, warnings, session and
verified image properties; its portable name is returned as `metadata_name`.
`--metadata embedded` instead writes the generation record into the PNG, JPEG,
or WebP itself using XMP without re-encoding pixels; `sidecar_and_embedded`
writes both. Embedded metadata also works with base64 responses. These modes
are explicit privacy choices because prompt and session content travel with the
image; metadata is disabled by default. For JPEG portability, embedded JSON is
bounded to 40 KiB and combined prompt text to 12 KiB, validated before provider
work.
`--preview` renders in supported Kitty/iTerm2-compatible terminals and degrades
to a status message elsewhere; `--open` launches the system image viewer.

### Reuse presets

Save and reuse complete settings across the CLI, API, and dashboard:

```sh
imagegen-bridge preset create portrait-high --from request.json
imagegen-bridge generate "A red-haired woman" --preset portrait-high
imagegen-bridge preset update portrait-high --from updated-template.json
imagegen-bridge preset delete portrait-high --force
```

A preset can retain its own prompt, or the prompt on `generate`/`edit` can
replace it. Image inputs, masks, reference-image bytes, and idempotency keys are
never retained in presets.

### Edit and use references

Edit an image or add visual references:

```sh
imagegen-bridge edit \
  "Change the jacket to dark blue" \
  --image ./portrait.png \
  --reference ./palette.png \
  --response-format artifact
```

### Reuse a Codex thread

Use a persistent app-server thread:

```sh
imagegen-bridge generate \
  "Create the first character sheet" \
  --session-key character-design \
  --response-format artifact

imagegen-bridge generate \
  "Keep the character and show a side view" \
  --session-key character-design \
  --response-format artifact
```

### Discover and validate capabilities

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

### Generate multiple images

Multi-image requests are available even when an upstream provider returns only
one image per call. Provider discovery reports the effective `count` plus a
`batching` object containing the native count, execution mode, and global
parallel limit. Codex app-server defaults to four logical outputs with two
isolated turns in flight (`providers.codex_app_server.max_outputs` and
`max_parallel_outputs`). Persistent-key and explicit-thread batches are
serialized to preserve conversation order. The Responses adapter supports the
same four-output contract and uses its own `max_parallel_outputs` setting.
`--batch-execution sequential` reproduces OpenClaw-style one-call-at-a-time
fan-out for an isolated request; `parallel` explicitly selects the configured
bounded concurrency, while `auto` remains session-aware.

`failure_policy=fail_fast` cancels outstanding work on the first failure.
`best_effort` returns successful images in requested-index order plus structured
`failures`; every success and failure includes its output index and per-item
generation time. Both direct API calls and durable dashboard jobs use this same
pipeline.

### Create transparent output

Request transparent output from the default GPT Image 2 app-server path:

```sh
imagegen-bridge generate \
  "A small red fox mascot, full body" \
  --background transparent \
  --transparency auto \
  --format png \
  --output mascots/fox.png \
  --metadata sidecar
```

`auto` uses provider-native alpha when available. Otherwise the bridge asks
the current model for a flat prompt-aware chroma background, samples the
actual generated border, removes it locally in Rust, normalizes fully
transparent pixels, cleans color spill, and rejects unusable alpha mattes.
`--transparency native` requires upstream alpha;
`--transparency chroma_key` forces local processing. Advanced callers can set
`--chroma-key`, both matte thresholds, and `--no-despill`. JPEG cannot carry
alpha and is rejected in strict mode or normalized to PNG under an explicit
compatibility policy.
Provider discovery reports this distinction as
`transparent_background=native|emulated|unsupported`; the raw `backgrounds`
set continues to describe values accepted directly by the upstream adapter.
This follows the generate-key-remove-validate strategy documented by OpenAI's
[Hatch Pet skill](https://github.com/openai/skills/blob/main/skills/.curated/hatch-pet/SKILL.md),
implemented here as a dependency-free Rust pipeline.

The same processor works without a provider call:

```sh
imagegen-bridge background remove keyed-input.png \
  --output transparent.png \
  --key auto
```

### Configure fallbacks

Provider fallback is opt-in and ordered:

```sh
imagegen-bridge generate "A red paper fox" \
  --provider codex-app-server \
  --fallback codex-responses:gpt-image-2 \
  --fallback-policy on_unavailable \
  --response-format artifact
```

`on_unavailable` handles unavailable authentication/configuration, capacity,
rate limits, and unsupported capabilities. `on_error` additionally permits
known-outcome protocol, upstream, or artifact failures. Neither mode reroutes
safety rejections, cancellation, permission errors, session failures, or an
operation whose upstream outcome is unknown. Fallbacks require isolated
sessions. Successful responses expose ordered `attempts`; sidecar metadata
retains the same trace.

### Use the experimental Responses transport

The Responses adapter forwards `action=auto|generate|edit`. The app-server path
accepts only `auto`. An explicit `input_fidelity=high` is accepted for
`gpt-image-2` but omitted upstream because that model already processes image
inputs at high fidelity; `low` is rejected. These rules follow the current
[OpenAI image generation contract](https://developers.openai.com/api/docs/guides/image-generation)
and are also published through provider discovery.

To use the experimental Responses provider, set
`providers.codex_responses.enabled = true` in the TOML configuration, then
select it explicitly:

```sh
imagegen-bridge generate \
  --provider codex-responses \
  --model gpt-image-1.5 \
  "A translucent red glass sculpture" \
  --background transparent \
  --format png \
  --response-format artifact
```

The full CLI reference, output modes, exit codes, configuration overrides,
session commands, schema commands, completions, and man-page generation are in
[docs/cli.md](docs/cli.md).

## HTTP service

The shortest path to the local UI is:

```sh
imagegen-bridge dashboard
```

This attaches to an Imagegen Bridge already listening at the configured local
address or starts one on loopback and opens the system browser when invoked
interactively. Use `--no-open` on headless systems, or `--attach-only --json`
when another program only needs discoverable connection details. A process
started by this command remains in the foreground until Ctrl-C/SIGINT.

To run the API explicitly, start the server on loopback:

```sh
imagegen-bridge serve --bind 127.0.0.1:8787
```

Then open `http://127.0.0.1:8787/dashboard`. The dashboard is served by the
same Rust process and needs no Node runtime, static-file server, CDN, or build
step. It supports generation and edit uploads, provider/model selection,
capability-aware controls, transparency processing, ordered fallback routes,
durable queue progress, cancellation confirmations,
verified transient partial previews, server-side prompt search, favorites,
hide/restore, verified thumbnails,
full-image viewing and download, portable artifact-path copy, reusable preset
CRUD, timings, revised prompts, raw retained metadata, model-specific
capability exploration, and a bounded redacted operator event history. Copied
paths are artifact-root-relative; the browser never receives a host filesystem
path. Controls that the selected provider cannot honor are disabled with an
explanation instead of submitting misleading values.
When bridge bearer authentication is enabled, enter the
token in the Connection dialog; it is kept in `sessionStorage` for that browser
tab and is never placed in a URL. Protected routes reject cross-site browser
requests and require an exact `Origin`/`Host` authority match whenever an
`Origin` header is present; CLI and SDK requests without browser origin headers
remain supported. The HTML shell contains no job or prompt data.

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

Queue a durable artifact-backed operation and inspect it later:

```sh
JOB_ID="$(curl --fail --silent --show-error \
  -H 'Content-Type: application/json' \
  -d '{"operation":"generate","prompt":"A small stone bridge in fog"}' \
  http://127.0.0.1:8787/v1/jobs | jq -r .id)"
curl --fail --silent --show-error "http://127.0.0.1:8787/v1/jobs/$JOB_ID"
```

Durable jobs always normalize delivery to bridge-owned artifacts so completed
history never stores large base64 outputs. The queue and running worker counts
are separately bounded. Queued work resumes after restart; work that was
already running is marked `interrupted` and is not automatically repeated
because the paid upstream operation may have completed.

Important routes:

| Route | Purpose |
| --- | --- |
| `GET /dashboard` | Embedded dependency-free generation and history UI |
| `POST /v1/images` | Lossless native generation/edit contract |
| `POST /v1/images/generations` | OpenAI-familiar JSON generation |
| `POST /v1/images/edits` | OpenAI-familiar multipart editing |
| `POST /v1/images/stream` | Bounded SSE progress/partial/completion stream |
| `POST /v1/jobs` | Create a durable asynchronous image operation |
| `GET /v1/jobs` | Cursor-paginated job history |
| `GET /v1/jobs/{id}` | Job state, request, result, and structured error |
| `DELETE /v1/jobs/{id}` | Request durable cancellation |
| `PATCH /v1/jobs/{id}` | Favorite, soft-delete, or restore a history item |
| `GET, POST /v1/presets` | List or create reusable request presets |
| `GET, PUT, DELETE /v1/presets/{name}` | Read, replace, or delete a preset |
| `GET /v1/artifacts/{id}` | Authenticated ownership-verified image bytes |
| `GET /v1/artifacts/{id}/thumbnail` | Bounded PNG thumbnail for galleries |
| `GET /v1/jobs/{id}/partial` | Latest verified transient preview for a running job |
| `GET /v1/providers` | Provider inventory |
| `GET /v1/providers/{provider}/capabilities` | Model-aware capabilities |
| `GET /v1/diagnostics` | Authenticated redaction-safe operator state |
| `GET /v1/sessions/{key}` | Persistent session lookup |
| `GET /health/live` | Process liveness |
| `GET /health/ready` | Detail-free cached provider readiness |
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
| Agent skill | `skills/generate-images-with-bridge` | Capability discovery, CLI invocation, verified absolute local artifact paths |
| Container | `Dockerfile`, `compose.yaml` | Bridge and pinned Codex CLI |

Examples and package build commands are in [docs/sdks.md](docs/sdks.md). The
OpenAI-familiar routes make migration from a subset of Images API calls small,
while the native route preserves sessions, normalizations, timings, warnings,
and verified artifact metadata.

The bundled agent skill is discoverable by the open Skills CLI and can be
installed for Codex or another supported agent:

```sh
npx skills add Crimsab/imagegen-bridge --skill generate-images-with-bridge
```

It discovers live capabilities, calls the local CLI, and returns verified
absolute artifact paths without reading OAuth credentials or depending on
OpenClaw.

## Configuration

Configuration is merged in this order:

```text
defaults < TOML file < IMAGEGEN_BRIDGE__* environment < --set/--unset
```

Nested environment keys use double underscores, for example:

```sh
export IMAGEGEN_BRIDGE__RUNTIME__GLOBAL__MAX_CONCURRENT=8
```

Without `--config`, commands prefer `./imagegen-bridge.toml`, then the user
configuration created under `XDG_CONFIG_HOME` (or `~/.config`). Unknown keys
fail validation. `config show` and `config origins` report effective settings
and provenance without resolving credential values. Start with `setup`, or use
[config.example.toml](config.example.toml) for a hand-managed deployment.

## Container

The image runs as UID/GID 10001, uses a read-only-compatible root filesystem,
and keeps OAuth state, SQLite state, artifacts, and reference inputs in separate
mounts. The Compose example binds the API to `127.0.0.1` by default.

```sh
export IMAGEGEN_BRIDGE_BEARER_TOKEN="$(openssl rand -hex 32)"
export IMAGEGEN_BRIDGE_CODEX_HOME="$PWD/deploy/codex-home"
docker compose up --build -d
```

Tagged releases also publish a multi-architecture image for Linux AMD64 and
ARM64 at `ghcr.io/crimsab/imagegen-bridge`.

Read [docs/deployment.md](docs/deployment.md) before mounting Codex credentials
or exposing the API on a network.

## Testing

The ordinary test suite uses fake Codex processes, mock HTTP servers, golden SSE
fixtures, and independently decoded image fixtures. The shared mock server also
mounts the real embedded dashboard for browser QA without paid generation.

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
```

Live Codex tests are ignored unless their explicit environment gates are set:
`IMAGEGEN_BRIDGE_LIVE_CODEX=1`, `IMAGEGEN_BRIDGE_LIVE_CODEX_RESPONSES=1`, or
`IMAGEGEN_BRIDGE_LIVE_BOOTSTRAP=1`. The exact offline, SDK, container, browser,
and live-OAuth matrix is documented in [docs/testing.md](docs/testing.md).
See [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.

## Security and upstream status

The direct Responses adapter uses a private ChatGPT/Codex endpoint. Its gated
live test performs a real advanced image request and verifies the returned
bytes; “experimental” describes upstream protocol stability, not missing test
coverage. The adapter may still stop working when that private protocol
changes. The app-server adapter remains the default.

Do not commit `auth.json`, mount an entire home directory into the container, or
bind an unauthenticated bridge to a public interface. Imagegen Bridge does not
disable or bypass upstream safety checks. Safety refusals are returned as
`safety_rejected` / `moderation_blocked` with a stable recovery hint to revise
the prompt or input images; the unchanged request is not retried automatically.

## License

Licensed under the [MIT License](LICENSE).
