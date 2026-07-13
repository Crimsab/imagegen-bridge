# Imagegen Bridge Python SDK

Typed sync and async clients for the normalized Imagegen Bridge HTTP API. The
package is not published yet; build it from this repository.

```python
from imagegen_bridge import AsyncImagegenBridgeClient, ImageRequest

async with AsyncImagegenBridgeClient("http://127.0.0.1:8787") as client:
    result = await client.images.generate(ImageRequest.generate("a paper fox"))
    print(result.data[0].b64_json)
```

Set `provider` in `ImageRequest.routing` to switch between configured bridge
providers; client construction and response types do not change.
