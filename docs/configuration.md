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
| `runtime.global.max_concurrent` | `"unlimited"` | Active bridge operations; use a positive integer to opt into admission control |
| `runtime.global.max_queued` | `"unlimited"` | Waiting-room capacity when concurrency is finite; `0` disables waiting |
| `runtime.provider_default.max_concurrent` | `"unlimited"` | Active calls per provider; use a positive integer for a caller-selected cap |
| `runtime.provider_default.max_queued` | `"unlimited"` | Provider waiting-room capacity when its concurrency is finite |
| `runtime.circuit_breaker.enabled` | `true` | Guard every provider independently |
| `runtime.circuit_breaker.failure_threshold` | `5` | Consecutive dependency failures before opening |
| `runtime.circuit_breaker.open_duration_ms` | `180000` | Recovery cooldown, independent of request duration |
| `runtime.circuit_breaker.half_open_max_calls` | `1` | Simultaneous recovery probes |
| `runtime.circuit_breaker.success_threshold` | `1` | Successful probes required to close |
| `runtime.request.max_outputs` | `255` | User-selected per-request ceiling; `255` is the v1 protocol maximum |
| `runtime.request.max_inputs` | `16` | Maximum source and reference inputs |
| `runtime.request.max_timeout_ms` | `1800000` | Highest accepted request deadline |

Admission fields accept either a positive integer or `"unlimited"`. Queue
capacity also accepts `0` for fail-fast behavior with no waiting room. Per-provider
overrides under `runtime.providers.<name>` take precedence over
`provider_default`. Provider/account rate limits still apply upstream; query
capabilities and inspect structured errors rather than assuming a browser UI's
private scheduling policy.

### Parallelism profiles

The defaults do not serialize independent work:

```toml
[runtime.global]
max_concurrent = "unlimited"
max_queued = "unlimited"

[runtime.provider_default]
max_concurrent = "unlimited"
max_queued = "unlimited"

[providers.codex_responses]
max_outputs = 255
max_parallel_outputs = "auto"
```

Consequently, `--count 5 --batch-execution parallel` starts five isolated
provider calls, and five independent API clients can also execute together.
No additional bridge process is needed. To opt into backpressure for a specific
deployment, choose finite values explicitly:

```toml
[runtime.providers.codex-responses]
max_concurrent = 8
max_queued = 0

[providers.codex_responses]
max_outputs = 32
max_parallel_outputs = 8
```

The equivalent one-shot override is
`--set 'providers.codex_responses.max_parallel_outputs=8'`. Use
`--batch-execution sequential` only when ordering or provider pressure matters;
explicit conversational app-server sessions remain sequential because their
turn history is shared state, unlike isolated image requests.

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
default image model is `gpt-image-2`, `max_outputs = 255`, and
`max_parallel_outputs = "auto"`: five requested outputs therefore start as five
independent upstream calls. Set `max_parallel_outputs` to a positive integer
only when you want to trade latency for lower provider pressure. Explicit safe
transient failures receive at most two total attempts. A
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
| `server.max_connections` | `"unlimited"` | Simultaneous connections; positive integer enables a listener cap |
| `server.read_timeout_ms` | `0` | Socket read-idle timeout, disabled at zero |
| `server.write_timeout_ms` | `30000` | Progress-based socket write stall timeout |
| `server.bearer_token_env` | unset | Environment variable holding the API bearer |
| `server.activation_lock` | unset | Cross-process singleton lock acquired before OAuth/provider bootstrap |

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

Durable-job worker and retention values remain explicit operator policy. They
can be raised independently from synchronous generation; measure provider
behavior and use error suggestions when selecting finite values.
