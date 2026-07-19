# Resilience model

Imagegen Bridge protects every integration point while leaving throughput
policy under operator control. Provider degradation is treated as normal
operational state. The release-readiness diagnostic is 8/8:

| Control | Implementation | Verification |
| --- | --- | --- |
| Outbound deadlines | One propagated request deadline plus transport and cancellation bounds | Runtime timeout and unknown-outcome tests |
| Circuit breakers | Independent closed/open/half-open state per provider | Breaker state-machine, fallback, and unknown-outcome tests |
| Configurable bulkheads | Global/per-provider admission accepts `unlimited` or operator-selected finite execution and queues | Unlimited admission plus finite stress tests |
| Zero-error handoff | OAuth-sterile gateway plus mutually exclusive blue/green slots | Gateway hold/switch test and Compose contract |
| Deep health | Cached provider readiness, state-store diagnostics, and slot readiness gates | Readiness and container smoke tests |
| Correlated telemetry | W3C `traceparent`, `x-request-id`, nested spans, JSON logs, bounded metrics | Header propagation and metrics tests |
| Beyond-peak load | Load, 3x-envelope stress, spike, and soak scenarios | Offline resilience harness and server stress test |
| Failure injection | 429/502, timeout, disconnect analogue, protocol, cancellation, and process fixtures | Deterministic offline fault matrix |

## Circuit breakers

Each registered provider has independent state. The default circuit opens after
five consecutive provider-system failures and permits one half-open probe after
three minutes. A successful probe closes it. The cooldown is configurable and
is not a request timeout.

Only dependency failures count: rate limiting, provider overload, timeout,
upstream failure, and invalid upstream protocol. Request validation, safety,
permissions, local input/artifact failures, authentication configuration, and
caller cancellation do not advance a closed circuit's failure counter. In
half-open state any non-success is conservative evidence that recovery is not
yet proven and reopens the circuit. A slow success—even one lasting several
minutes—resets the failure sequence.

An unknown post-dispatch outcome may open the circuit for later calls, but the
current request is never retried or sent to a fallback. This avoids duplicate
paid generations. An already-open primary fails fast with `503`, a bounded
`Retry-After`, and may use an explicitly configured fallback.

```toml
[runtime.circuit_breaker]
enabled = true
failure_threshold = 5
open_duration_ms = 180000
half_open_max_calls = 1
success_threshold = 1

[runtime.circuit_breakers.codex-responses]
enabled = true
failure_threshold = 4
open_duration_ms = 240000
half_open_max_calls = 1
success_threshold = 1
```

## Telemetry

HTTP clients may send a safe `x-request-id` and a W3C version-00
`traceparent`. The bridge returns both, preserves the trace ID, creates a new
server span ID, records the inbound parent span ID, and nests image-operation
and provider-attempt spans beneath the HTTP span. JSON tracing emits span
creation/closure plus explicit trace IDs on completion/failure logs; the stable
gateway forwards `traceparent` downstream. This is vendor-neutral structured
trace output rather than a bundled OTLP exporter. Completion logs include
effective provider, total duration, provider duration, and queue duration.
Prometheus labels remain deliberately
low-cardinality: trace and request IDs never become labels.

Authenticated diagnostics expose queue and breaker snapshots. Prometheus adds
fixed duration buckets suitable for p50/p95/p99 calculation, effective-provider
outcomes, queue depth, provider restarts, circuit one-hot state, transitions,
and rejections.

## Operational interpretation

A breaker opening is expected protection; alert when it remains open or cycles
repeatedly. Provider duration near total duration with queue duration near zero
means the bridge is not the bottleneck. Rising queue duration or overload with
stable provider latency means configured concurrency is saturated.

The handoff profile also configures a shared activation lock in the durable
state volume. The bridge must
acquire it before reading OAuth state, spawning Codex, or building providers.
Kernel lock ownership is process-scoped and releases on crash, preventing an
operator mistake from making both blue and green OAuth-active.
