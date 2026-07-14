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
  routing: {
    provider: "codex-app-server",
    fallbacks: [{ provider: "codex-responses", model: "gpt-image-2" }],
  },
  output: {
    response_format: "artifact",
    directory: "illustrations",
    filename: "fox.png",
    collision: "suffix",
    metadata: "sidecar",
  },
});
const queued = await bridge.jobs.create({ operation: "generate", prompt: "a second paper fox" });
const partial = await bridge.jobs.partial(queued.id); // Uint8Array; transient while running
const completed = await bridge.jobs.get(queued.id);
const page = await bridge.jobs.list({
  status: "succeeded",
  visibility: "active",
  favorite: true,
  search: "paper fox",
});
const diagnostics = await bridge.diagnostics();
console.log(diagnostics.configuration.listener_scope, diagnostics.providers);
```

Changing `routing.provider` is the only SDK change needed to select another
configured provider.
`routing.fallbacks` adds ordered provider/model alternatives; responses expose
every attempted route. `output.transparency` controls native or local
chroma-key alpha while `parameters.background` remains `transparent`.
`output.metadata` accepts `none`, `sidecar`, `embedded`, or
`sidecar_and_embedded`. Embedded XMP is carried inside the returned PNG, JPEG,
or WebP bytes; the latter combined mode requires artifact output.
`bridge.jobs` exposes typed `create`, `get`, `list`, `partial`, `cancel`, and
`update` operations for durable artifact-backed work. `partial` returns the
latest verified in-memory preview and normally returns 404 before the first
partial event or after the job becomes terminal.
List filters include stable cursor pagination, status, active/hidden/all
visibility, favorite state, and literal prompt search.
`diagnostics()` exposes the typed, redaction-safe operator snapshot used by the
embedded dashboard without credential values, prompts, or host paths.

Plain HTTP is accepted only for literal loopback addresses and `localhost`.
Remote bridge URLs must use HTTPS unless `allowInsecureRemoteHttp: true` is set
explicitly; that opt-in removes transport confidentiality and server
authentication. Redirects are rejected so authorization is never forwarded to
an unvalidated destination.

`timeoutMs`, `maxJsonBodyBytes`, `maxErrorBodyBytes`, `maxSseLineBytes`, and
`maxSseEventBytes` provide independent transport bounds. Partial previews have
a fixed 16 MiB streamed ceiling. Image and durable-job creation both accept
`RequestOptions.idempotencyKey` and fall back to the key in the request body.
