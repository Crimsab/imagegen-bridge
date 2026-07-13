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
```

Use `client.images.stream(request)` for typed lifecycle, progress, partial-image,
completion, and error events. `BridgeAPIError` exposes HTTP status, standard
error fields, stable bridge code, retryability, safe provider/upstream IDs,
details, and request ID.

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
```

Pass `{ signal }` or `{ timeoutMs }` per request. Streaming is an
`AsyncIterable<StreamEvent>` and cancels the HTTP body when iteration ends.
Both SDKs expose requested output indices, optional per-item generation time,
and structured failures from best-effort multi-image requests.

## Contract verification and packaging

Python and TypeScript tests launch the same Rust black-box server in
`tools/sdk-mock-server`. It consumes fixtures from `fixtures/sdk`, and the
fixture server itself deserializes request, response, capability, and session
fixtures with the Rust domain types. This catches drift in either direction.

CI validates Rust docs/types, Python Ruff+mypy+pytest+wheel/sdist, TypeScript
Biome+strict TypeScript+Bun tests+Node smoke test+package contents. Packages are
built and inspected in CI, but none are published yet.
