# SDK guide

All SDKs consume the provider-neutral native API. Provider selection lives in
the request (`routing.provider`), so moving between `codex-app-server` and the
experimental `codex-responses` adapter does not require a new client, package,
authentication flow, or response model. Bridge bearer authentication remains
separate from Codex OAuth, which is owned by the bridge process.

## Rust

The `imagegen-bridge` crate re-exports the versioned core, configuration,
runtime, and artifact crates. `BridgeApplication::from_config` assembles enabled
first-party providers; `BridgeApplication::builder` accepts third-party
`ImageProvider` implementations while retaining the common runtime.

```rust,no_run
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
clients, bounded SSE streaming, deadlines, structured errors, and every native
discovery/session endpoint. It ships `py.typed`, frozen dataclass models, and
wheel/sdist metadata.

```python
from imagegen_bridge import AsyncImagegenBridgeClient, ImageRequest, RoutingOptions

request = ImageRequest.generate(
    "a paper fox",
    routing=RoutingOptions(provider="codex-app-server"),
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
```

Use `client.images.stream(request)` for typed lifecycle, progress, partial-image,
completion, and error events. `BridgeAPIError` exposes HTTP status, standard
error fields, stable bridge code, retryability, safe provider/upstream IDs,
details, and request ID.
`client.jobs` provides typed create/get/list/cancel operations for durable
artifact-backed work plus `update` for favorite/delete/restore state; job pages
never duplicate inline request image bodies.
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

```ts
import { ImagegenBridgeClient } from "@imagegen-bridge/typescript";

const client = new ImagegenBridgeClient({
  baseUrl: "http://127.0.0.1:8787",
  bearerToken: "bridge-token",
});
const response = await client.images.generate({
  operation: "generate",
  prompt: "a paper fox",
  routing: { provider: "codex-app-server" },
});
const job = await client.jobs.create({ operation: "generate", prompt: "a paper fox" });
const completed = await client.jobs.get(job.id);
const favorites = await client.jobs.list({
  visibility: "active",
  favorite: true,
  search: "paper fox",
});
const diagnostics = await client.diagnostics();
```

Pass `{ signal }` or `{ timeoutMs }` per request. Streaming is an
`AsyncIterable<StreamEvent>` and cancels the HTTP body when iteration ends.
Remote plaintext HTTP requires `allowInsecureRemoteHttp: true`; redirects are
rejected. JSON/error body limits and byte-framed SSE limits are independently
configurable, while partial previews retain a fixed streamed 16 MiB ceiling.
Both SDKs expose durable jobs, requested output indices, optional per-item generation time,
and structured failures from best-effort multi-image requests.
The request and capability types also expose input fidelity, image action, and
the provider-specific accepted value sets instead of assuming every model can
honor every edit control. Provider descriptors expose their declared image
models so clients can build model pickers without hardcoded inventories.

## Contract verification and packaging

Python and TypeScript tests launch the same Rust black-box server in
`tools/sdk-mock-server`. It consumes fixtures from `fixtures/sdk`, and the
fixture server itself deserializes request, response, capability, and session
fixtures with the Rust domain types. This catches drift in either direction.

CI validates Rust docs/types, Python Ruff+mypy+pytest+wheel/sdist, TypeScript
Biome+strict TypeScript+Bun tests+Node smoke test+package contents. Packages are
built and inspected in CI, but none are published yet. Python build isolation
uses an exact backend and a committed hash-checked closure; external GitHub
Actions are pinned to reviewed full commit SHAs.
