# Imagegen Bridge Python SDK

Typed sync and async clients for the normalized Imagegen Bridge HTTP API. The
package targets Python 3.10 and newer.

```sh
uv add imagegen-bridge
```

```python
from imagegen_bridge import (
    AsyncImagegenBridgeClient,
    ImagePresetCreate,
    ImagePresetTemplate,
    ImageRequest,
    OutputOptions,
    ProviderRoute,
    RoutingOptions,
)

async with AsyncImagegenBridgeClient("http://127.0.0.1:8787") as client:
    result = await client.images.generate(ImageRequest.generate(
        "a paper fox",
        output=OutputOptions(
            response_format="artifact",
            directory="illustrations",
            filename="fox.png",
            collision="suffix",
            metadata="sidecar",
        ),
        routing=RoutingOptions(
            provider="codex-responses",
            fallbacks=(ProviderRoute("codex-app-server", "gpt-image-2"),),
        ),
    ))
    print(result.data[0].name, result.data[0].metadata_name)

    queued = await client.jobs.create(ImageRequest.generate("a second paper fox"))
    partial = await client.jobs.partial(queued.id)  # transient while the job runs
    completed = await client.jobs.get(queued.id)
    preset = await client.presets.create(ImagePresetCreate(
        name="paper-fox",
        template=ImagePresetTemplate(prompt="a paper fox"),
    ))
    page = await client.jobs.list(
        status="succeeded", visibility="active", favorite=True, search="paper fox"
    )
    diagnostics = await client.diagnostics()
    print(diagnostics.configuration.listener_scope, diagnostics.providers)
```

Set `provider` in `ImageRequest.routing` to switch between configured bridge
providers; client construction and response types do not change.
Fallback routes are ordered and returned as typed provider attempts.
`OutputOptions.transparency` selects native or local chroma-key alpha when the
generation parameters request a transparent background.
`OutputOptions.metadata` accepts `none`, `sidecar`, `embedded`, or
`sidecar_and_embedded`. Embedded XMP is carried inside the returned PNG, JPEG,
or WebP bytes; the latter combined mode requires artifact output.
`client.jobs` is also available on the synchronous client and exposes
`create`, `get`, `list`, `partial`, `cancel`, and `update` with typed durable job
models. `partial` returns the latest verified in-memory preview and normally
returns 404 before the first partial event or after the job becomes terminal.
List filters include stable cursor pagination, status, active/hidden/all
visibility, favorite state, and literal prompt search.
`client.presets` exposes typed `list`, `get`, `create`, `update`, and `delete`
operations for reusable input-free request configurations.
`diagnostics()` returns the same typed, redaction-safe operator snapshot used by
the embedded dashboard; it never includes credential values, prompts, or host
paths.

Plain HTTP is accepted only for literal loopback addresses and `localhost`.
Remote bridge URLs must use HTTPS unless the caller explicitly sets
`allow_insecure_remote_http=True`, which disables transport confidentiality and
server authentication. Redirects remain governed by HTTPX and should not be
enabled for credential-bearing clients.

Per-call `timeout` omission inherits the timeout configured on the client;
passing `None` explicitly disables HTTPX timeouts for that call, while a number
or `httpx.Timeout` overrides them. Ordinary JSON bodies, error bodies, partial
previews, SSE lines, and aggregate SSE events have independent configurable
limits and are counted while streaming. `Idempotency-Key` can be supplied with
`idempotency_key=` to both image and durable-job creation methods.
