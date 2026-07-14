# Imagegen Bridge Python SDK

Typed sync and async clients for the normalized Imagegen Bridge HTTP API. The
package is not published yet; build it from this repository.

```python
from imagegen_bridge import AsyncImagegenBridgeClient, ImageRequest, OutputOptions

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
    ))
    print(result.data[0].name, result.data[0].metadata_name)

    queued = await client.jobs.create(ImageRequest.generate("a second paper fox"))
    partial = await client.jobs.partial(queued.id)  # transient while the job runs
    completed = await client.jobs.get(queued.id)
    page = await client.jobs.list(
        status="succeeded", visibility="active", favorite=True, search="paper fox"
    )
    diagnostics = await client.diagnostics()
    print(diagnostics.configuration.listener_scope, diagnostics.providers)
```

Set `provider` in `ImageRequest.routing` to switch between configured bridge
providers; client construction and response types do not change.
`client.jobs` is also available on the synchronous client and exposes
`create`, `get`, `list`, `partial`, `cancel`, and `update` with typed durable job
models. `partial` returns the latest verified in-memory preview and normally
returns 404 before the first partial event or after the job becomes terminal.
List filters include stable cursor pagination, status, active/hidden/all
visibility, favorite state, and literal prompt search.
`diagnostics()` returns the same typed, redaction-safe operator snapshot used by
the embedded dashboard; it never includes credential values, prompts, or host
paths.
