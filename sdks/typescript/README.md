# Imagegen Bridge TypeScript SDK

Dependency-free ESM client for Bun and Node 20+, with strict request/response
types, `AbortSignal`, request deadlines, bounded SSE parsing, and structured
errors. Publication is disabled while the repository is private.

```ts
import { ImagegenBridgeClient } from "@imagegen-bridge/typescript";

const bridge = new ImagegenBridgeClient({ baseUrl: "http://127.0.0.1:8787" });
const result = await bridge.images.generate({
  operation: "generate",
  prompt: "a paper fox",
  routing: { provider: "codex-app-server" },
});
```

Changing `routing.provider` is the only SDK change needed to select another
configured provider.
