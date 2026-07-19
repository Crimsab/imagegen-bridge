# Frequently asked questions

## Why can three images take several minutes?

Generation is provider-latency-bound. The current default
`max_parallel_outputs = "auto"` starts all requested outputs concurrently.
An operator-selected value such as `2` deliberately creates multiple waves;
observed medium-quality provider calls have taken roughly 95–119 seconds each.
Inspect response `timings`: `provider_ms` near
`total_ms` with `queue_ms` near zero means the bridge is not the bottleneck.
Browser ChatGPT can use different defaults and infrastructure.

## Does the bridge require an OpenAI API key?

No. The default `codex-responses` provider uses your existing Codex and ChatGPT
OAuth session. The reserved OpenAI Platform provider is separate and disabled.

## Is the bridge public by default?

No. The example server binds to `127.0.0.1:8787`. The Compose profile also binds
to host loopback. Use bearer authentication and a trusted TLS proxy before
exposing it beyond a private interface.

## Is the bridge bearer the same as Codex OAuth?

No. OAuth authenticates the bridge to Codex. The bearer authenticates your
applications to the bridge. Store and rotate them independently.

## Which provider should I use?

Use `codex-responses` unless you need a compatibility behavior that exists only
in `codex-app-server`. Query live capabilities for the selected model rather
than choosing by provider name alone.

## Can it edit images and use references?

Yes. Both Codex transports support contextual image inputs according to their
live capability documents. Raster masks are present in the contract but are
currently rejected as unsupported by both transports.

## Will a failed request be retried?

Only an explicit, pre-output transient failure can be retried. Safety,
permission, authentication, cancellation, session, idempotency, and unknown
outcomes are never automatically rerouted or repeated.

## Are prompts or images written to logs?

No supported tracing mode logs prompts, source images, response bodies,
credentials, account identifiers, or filesystem paths. Durable jobs and
explicit metadata modes can persist request content by design, so configure
their storage and retention accordingly.

## Where does the dashboard store its token?

Only in the current browser tab's `sessionStorage`. Artifact and history calls
remain authenticated, and the bearer is not placed in image URLs.

## Can several applications share one bridge?

Yes. One process handles independent requests concurrently; multiple bridge
processes are not required. Admission defaults to `"unlimited"`, while finite
global/provider values and queues are opt-in. One bridge bearer still defines
one history ownership scope, and the upstream account may enforce its own rate
limits.

## What survives a restart?

Presets, sessions, durable jobs, and artifacts survive when their configured
storage is persistent. Queued jobs resume. Previously running jobs become
`interrupted` and are not silently repeated.

## Can I keep private deployment overrides in the repository checkout?

Yes, but keep them untracked. The repository ignores conventional local and
private Compose and TOML override names, `.env`, database files, artifacts, and
the dedicated Codex home. A stronger deployment can store all private
overrides outside the public checkout.
