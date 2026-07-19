# Configuration reference

Imagegen Bridge uses versioned TOML. Unknown keys and invalid values fail before
the service mutates storage.

## Precedence

Configuration is resolved in this order, from lowest to highest priority:

```text
built-in defaults < TOML file < IMAGEGEN_BRIDGE__* environment < --set/--unset
```

Use `config origins` to see where each effective field came from. Credential
values are never resolved into that output.

```sh
imagegen-bridge --config ./config.toml config check
imagegen-bridge --config ./config.toml config show
imagegen-bridge --config ./config.toml config origins
```

The complete example is [`config.example.toml`](https://github.com/Crimsab/imagegen-bridge/blob/main/config.example.toml).

## Runtime and admission

| Field | Default | Purpose |
| --- | ---: | --- |
| `runtime.default_timeout_ms` | `300000` | End-to-end request deadline |
| `runtime.global.max_concurrent` | `16` | Maximum active bridge operations |
| `runtime.global.max_queued` | `64` | Global waiting-room capacity |
| `runtime.provider_default.max_concurrent` | `4` | Default active calls per provider |
| `runtime.provider_default.max_queued` | `16` | Default provider queue capacity |
| `runtime.request.max_outputs` | `16` | Maximum normalized outputs per request |
| `runtime.request.max_inputs` | `16` | Maximum source and reference inputs |
| `runtime.request.max_timeout_ms` | `1800000` | Highest accepted request deadline |

Provider capabilities can impose tighter limits. Always query capabilities for
the selected provider and model.

## Inputs

`inputs.local_roots` is the allowlist for local paths used by native CLI and
embedded callers. HTTP clients cannot submit arbitrary server paths.

Remote inputs are disabled by default. Enabling them requires an explicit host
and port allowlist. Private networks stay blocked unless
`inputs.remote.allow_private_networks` is deliberately enabled.

## Artifacts

| Field | Default | Purpose |
| --- | ---: | --- |
| `artifacts.root` | `./data/artifacts` | Bridge-owned output root |
| `artifacts.image.max_encoded_bytes` | `33554432` | Maximum encoded image size |
| `artifacts.image.max_pixels` | `67108864` | Decode bomb protection |
| `artifacts.retention.max_age_secs` | `604800` | Ordinary artifact retention |

Output directories and filenames are portable relative paths below the
artifact root. Publication is atomic and never overwrites by default.

## Providers

`codex-responses` is enabled by default and uses Codex OAuth directly. Its
default image model is `gpt-image-2`, provider-wide output parallelism is `2`,
and explicit safe transient failures receive at most two total attempts. A
failed `image_generation_call` is a known failed outcome and receives one
retry; refusals and unknown post-dispatch outcomes remain terminal.

`codex-app-server` is a compatibility transport. It supervises the pinned or
configured Codex executable and stores reusable session bindings in SQLite.
Because a successful app-server turn can complete without emitting an image
item, do not configure it as an automatic production fallback for
`codex-responses`.

The `providers.openai` section reserves an API-key-backed integration surface.
It is disabled and not registered by the current release.

## HTTP server

| Field | Default | Purpose |
| --- | ---: | --- |
| `server.bind` | `127.0.0.1:8787` | Listener address |
| `server.max_body_bytes` | `83886080` | Request body ceiling |
| `server.max_connections` | `256` | Simultaneous connections |
| `server.read_timeout_ms` | `0` | Socket read-idle timeout, disabled at zero |
| `server.write_timeout_ms` | `30000` | Progress-based socket write stall timeout |
| `server.bearer_token_env` | unset | Environment variable holding the API bearer |

The runtime request deadline limits generation duration. Socket timeouts protect
network progress and are not substitutes for that deadline.

## Durable jobs

| Field | Default | Purpose |
| --- | ---: | --- |
| `server.jobs.max_pending` | `1000` | Maximum queued jobs |
| `server.jobs.max_running` | `4` | Job workers |
| `server.jobs.retention_secs` | `604800` | Terminal history age |
| `server.jobs.max_retained` | `10000` | Terminal record count |
| `server.jobs.max_retained_bytes` | `268435456` | Logical history budget |
| `server.jobs.max_database_bytes` | `1073741824` | Global admission budget |

Small private deployments can reduce queue and history limits. Do not raise
worker counts above the useful provider concurrency without measuring a real
workload.
