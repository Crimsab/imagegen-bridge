# SDK guide

All SDKs consume the provider-neutral native API. Provider selection lives in
the request (`routing.provider`), so moving between `codex-app-server` and the
default `codex-responses` adapter does not require a new client, package,
authentication flow, or response model. Bridge bearer authentication remains
separate from Codex/ChatGPT OAuth, which is owned by the bridge process.
`codex-responses` never uses `OPENAI_API_KEY`; an official Platform API-key
provider is a separate reserved integration.

## Rust

The `imagegen-bridge` crate re-exports the versioned core, configuration,
runtime, and artifact crates. `BridgeApplication::from_config` assembles enabled
first-party providers; `BridgeApplication::builder` accepts third-party
`ImageProvider` implementations while retaining the common runtime.

```sh
cargo add imagegen-bridge
```

```rust
use imagegen_bridge::{BridgeApplication, config::BridgeConfig};

# async fn example(config: BridgeConfig) -> Result<(), imagegen_bridge::core::BridgeError> {
let bridge = BridgeApplication::from_config(config).await?;
let response = bridge
    .runtime()
    .execute(imagegen_bridge::core::ImageRequest::generate("a paper fox"))
    .await?;
bridge.shutdown().await?;
# Ok(())
# }
```

## Python

The package in `sdks/python` supports Python 3.10+, sync and async HTTPX
clients, bounded SSE streaming, deadlines, structured errors with recovery
suggestions, and every native
discovery/session endpoint. It ships `py.typed`, frozen dataclass models, and
wheel/sdist metadata.

```sh
uv add imagegen-bridge
```

```python
from imagegen_bridge import (
    AsyncImagegenBridgeClient,
    ImagePresetCreate,
    ImagePresetTemplate,
    ImageRequest,
    ProviderRoute,
    RoutingOptions,
)

request = ImageRequest.generate(
    "a paper fox",
    routing=RoutingOptions(
        provider="codex-responses",
        fallbacks=(ProviderRoute("codex-app-server", "gpt-image-2"),),
    ),
)
async with AsyncImagegenBridgeClient(
    "http://127.0.0.1:8787",
    bearer_token="bridge-token",
) as client:
    response = await client.images.generate(request)
    job = await client.jobs.create(request)
    completed = await client.jobs.get(job.id)
    favorites = await client.jobs.list(
        visibility="active", favorite=True, search="paper fox"
    )
    diagnostics = await client.diagnostics()
    preset = await client.presets.create(ImagePresetCreate(
        name="portrait-high",
        template=ImagePresetTemplate(prompt="a studio portrait"),
    ))
    presets = await client.presets.list()
```

Use `client.images.stream(request)` for typed lifecycle, progress, partial-image,
completion, and error events. `BridgeAPIError` exposes HTTP status, standard
error fields, stable bridge code, retryability, safe provider/upstream IDs,
details, and request ID.
`client.jobs` provides typed create/get/list/cancel operations for durable
artifact-backed work plus `update` for favorite/delete/restore state; job pages
never duplicate inline request image bodies.
`client.presets` exposes typed `list`, `get`, `create`, `update`, and `delete`
operations over the same reusable configurations as the CLI and dashboard.
`diagnostics()` returns typed aggregate health, safe configuration provenance,
queue/storage limits, provider readiness, and bounded redacted API events
without user content, identifiers, headers, queries, payloads, or host paths.
Omitting a per-call Python timeout inherits the client timeout; explicit `None`
disables it. Remote plaintext HTTP is rejected unless the constructor receives
the conspicuous unsafe override. JSON, error, partial-preview, SSE-line, and
SSE-event bytes are limited before decoding or coalescing.

## TypeScript

The dependency-free ESM package in `sdks/typescript` targets Bun 1.2+ and Node
20+. It uses the runtime Fetch API, supports `AbortSignal`, request deadlines,
bounded fragmented SSE, strict declarations, and structured errors.

```sh
bun add imagegen-bridge
```

```ts
import { ImagegenBridgeClient } from "imagegen-bridge";

const client = new ImagegenBridgeClient({
  baseUrl: "http://127.0.0.1:8787",
  bearerToken: "bridge-token",
});
const response = await client.images.generate({
  operation: "generate",
  prompt: "a paper fox",
  routing: {
    provider: "codex-responses",
    fallbacks: [{ provider: "codex-app-server", model: "gpt-image-2" }],
  },
});
const job = await client.jobs.create({ operation: "generate", prompt: "a paper fox" });
const completed = await client.jobs.get(job.id);
const favorites = await client.jobs.list({
  visibility: "active",
  favorite: true,
  search: "paper fox",
});
const diagnostics = await client.diagnostics();
const preset = await client.presets.create({
  name: "portrait-high",
  template: { prompt: "a studio portrait", parameters: { quality: "high" } },
});
const presets = await client.presets.list();
```

Pass `{ signal }` or `{ timeoutMs }` per request. Streaming is an
`AsyncIterable<StreamEvent>` and cancels the HTTP body when iteration ends.
Remote plaintext HTTP requires `allowInsecureRemoteHttp: true`; redirects are
rejected. JSON/error body limits and byte-framed SSE limits are independently
configurable, while partial previews retain a fixed streamed 16 MiB ceiling.
Both SDKs expose durable jobs, requested output indices, optional per-item generation time,
and structured failures from best-effort multi-image requests.
They also expose transparent-output controls and ordered provider-attempt traces;
changing provider routes does not change the client or response type.
The request and capability types also expose input fidelity, image action, and
the provider-specific accepted value sets instead of assuming every model can
honor every edit control. Provider descriptors expose their declared image
models so clients can build model pickers without hardcoded inventories.
Both SDKs also expose complete preset CRUD; preset templates intentionally
exclude one-shot image inputs and idempotency state.

## Contract verification and packaging

Python and TypeScript tests launch the same Rust black-box server in
`tools/sdk-mock-server`. It consumes fixtures from `fixtures/sdk`, and the
fixture server itself deserializes request, response, capability, and session
fixtures with the Rust domain types. This catches drift in either direction.

CI validates Rust docs/types and publishable crates, Python
Ruff+mypy+pytest+wheel/sdist, and TypeScript Biome+strict TypeScript+Bun
tests+Node smoke test+package contents. A tagged GitHub Release publishes each
registry package from the same versioned source. Python build isolation uses an
exact backend and a committed hash-checked closure; external GitHub Actions are
pinned to reviewed full commit SHAs.
