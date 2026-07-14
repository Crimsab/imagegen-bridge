# Imagegen Bridge TypeScript SDK

Dependency-free ESM client for Bun and Node 20+, with strict request/response
types, `AbortSignal`, request deadlines, bounded SSE parsing, and structured
errors. The package is not published yet; build it from this repository.

```ts
import { ImagegenBridgeClient } from "@imagegen-bridge/typescript";

const bridge = new ImagegenBridgeClient({ baseUrl: "http://127.0.0.1:8787" });
const result = await bridge.images.generate({
  operation: "generate",
  prompt: "a paper fox",
  routing: { provider: "codex-app-server" },
  output: {
    response_format: "artifact",
    directory: "illustrations",
    filename: "fox.png",
    collision: "suffix",
    metadata: "sidecar",
  },
});
const queued = await bridge.jobs.create({ operation: "generate", prompt: "a second paper fox" });
const completed = await bridge.jobs.get(queued.id);
const page = await bridge.jobs.list({ status: "succeeded" });
const diagnostics = await bridge.diagnostics();
console.log(diagnostics.configuration.listener_scope, diagnostics.providers);
```

Changing `routing.provider` is the only SDK change needed to select another
configured provider.
`bridge.jobs` exposes typed `create`, `get`, `list`, `cancel`, and `update` operations for
durable artifact-backed work.
`diagnostics()` exposes the typed, redaction-safe operator snapshot used by the
embedded dashboard without credential values, prompts, or host paths.
