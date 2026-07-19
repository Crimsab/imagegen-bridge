# Limits and guarantees

The bridge deliberately separates what it accepts, what one provider call can
produce, and what it may execute concurrently.

## Capability negotiation

`GET /v1/providers/{name}/capabilities` is the authority for model features and
limits. A request can accept several outputs while a provider supports only one
output per upstream call. In that case the bridge uses independent fan-out and
restores the original output order.

Capabilities can change independently of the versioned API contract. Consumers
should cache them briefly and refresh after upgrades or capability errors.

## Concurrency and queues

Admission happens at three levels:

1. The global runtime may limit active and queued operations.
2. Each provider may have its own active-call and waiting-room policy.
3. Durable jobs have separate explicit pending and worker policies.

Global/provider admission defaults to `"unlimited"`. Set a positive
`max_concurrent` only when you want a bridge-side bulkhead; set
`max_queued = 0` for fail-fast or `"unlimited"` for no bridge-side waiting-room
ceiling. `max_parallel_outputs = "auto"` runs every output in one logical
request concurrently, while a positive integer deliberately caps that fan-out.
These settings do not change provider-side account limits.

## Retry boundary

The Codex Responses transport allows at most two total attempts. It retries only
an explicit transient failure that occurred before any output and is known to
be safe.

The bridge never automatically retries:

- safety or moderation refusals;
- authentication or permission errors;
- cancellation;
- session and idempotency conflicts;
- a timeout or disconnect after upstream dispatch;
- any result whose completion is unknown.

Structured errors expose `retryable`, ordered provider attempts, diagnostics,
and ordered `suggestions` so callers can show concrete recovery actions without
guessing from HTTP status text.

## Idempotency

`Idempotency-Key` on synchronous POST routes is process-local. The bridge keeps
bounded completed and in-flight entries for the configured lifetime.

For durable job creation, idempotency is persisted. Reusing the same key,
authorization scope, and request returns the original job. Reusing the key with
a different request returns `409`.

Bearer rotation creates a new ownership scope. It does not grant the new token
access to job history owned by the previous bearer.

## Crash and restart behavior

Queued durable jobs resume after restart. Jobs that were running become
`interrupted` and are not restarted automatically because upstream completion
may be ambiguous.

Synchronous requests are not durable. A client disconnect cancels work when
the active provider can do so cooperatively, but callers must still treat a
post-dispatch transport failure as an unknown outcome.

## Artifacts and history

Artifact delivery verifies ownership, checksum, and full image decoding before
returning bytes. Paths are bridge-owned and callers receive opaque identifiers
or portable relative names, never absolute server paths.

Job history and artifact bytes have separate retention policies. Favoriting a
job exempts its record from ordinary history pruning, but does not make its
artifact immortal.

## Request sizes

The service bounds body size, prompt bytes, source count, output count, encoded
image size, decoded pixels, dimensions, identifiers, metadata, and provider
responses. These checks happen before expensive work wherever possible.

Consult [configuration](configuration.md) for defaults and the live capability
document for tighter provider-specific constraints.
