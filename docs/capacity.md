# Capacity and failure testing

Image generation is latency-bound, so capacity is primarily a concurrency
model rather than a conventional high-RPS model.

```text
sustainable operations/second ≈ provider concurrency / p95 provider seconds
finite provider admission envelope = max_concurrent + max_queued
finite global admission envelope = max_concurrent + max_queued
```

The default profile uses `"unlimited"`, so the bridge does not create an
admission envelope of its own. The upstream service and host resources remain
real constraints. Operators who prefer overload shedding can set finite
global and provider values from measured latency and account/provider limits,
then keep client retries outside the bridge's bounded retry budget.

## Offline harness

The Bun harness defaults to a deterministic local synthetic dependency. It
does not read OAuth state or generate images. Every explicit `--url`, including
loopback, is rejected unless `--allow-live` is supplied because it may trigger
real paid/subscription-backed generations.

```sh
bun run tools/resilience-harness.ts --scenario load --json
bun run tools/resilience-harness.ts --scenario stress --json
bun run tools/resilience-harness.ts --scenario spike --json
bun run tools/resilience-harness.ts --scenario soak --soak-seconds 300 --json
bun run tools/resilience-harness.ts --scenario faults --json
```

The harness deliberately selects a finite synthetic envelope of 4 active plus
16 queued; it does not inherit the unlimited product default. Stress submits 60
concurrent operations—3x that envelope—and must degrade through fast 503s,
remain live, drain every queue slot, and accept a recovery probe. Spike repeats
the sudden burst. Soak runs at expected load. Faults inject 502, 429,
disconnect analogue, and a hung call at the synthetic dependency boundary; it
does not by itself claim to exercise bridge routing.
Stress and spike pass only when both admitted successes and bounded overloads
occur, all results stay inside the modeled envelope, and a recovery probe
succeeds. The transport fault matrix is complemented by Rust integration tests
that exercise bridge timeout, process exit, protocol failure, breaker, and
fallback behavior through the real runtime.

A shortened pass is part of normal CI. The scheduled workflow runs the
five-minute soak. Longer 1–24 hour runs should be started manually before
choosing finite concurrency limits.

## Example synthetic baseline

On 2026-07-19, seed 7 with a 15–25 ms synthetic dependency produced:

| Scenario | Result |
| --- | --- |
| Load, concurrency 4 | 40/40 success; p95 24 ms |
| Stress, concurrency 60 | 20 admitted, 100 bounded overloads; recovery passed |
| Spike, concurrency 80 | 20 admitted, 60 bounded overloads; recovery passed |
| Abbreviated soak | 400/400 success; recovery passed |
| Fault matrix | Four expected failures; recovery passed |

This baseline proves the opt-in finite-capacity profile, not Codex throughput. Measure real
provider p95 separately; do not load-test subscription-backed generation
without explicit authorization.

## Observed Codex latency

On the same host, three medium-quality live outputs observed before 0.2.0 took
94.8–119.3 seconds each, with 329.4 seconds reported by the provider across the
batch and effectively zero bridge queue time. A final 0.2.0 low-quality
single-output smoke took 42.27 seconds end to end: 42.26 seconds provider time,
0 milliseconds queue time, and about 10 milliseconds artifact/bridge work.

These samples are not an SLA. They were captured with the previous
`max_parallel_outputs = 2`, where three outputs required two provider waves.
The current `"auto"` default launches the three calls together. Browser ChatGPT
may still use different orchestration, capacity, defaults, or
presentation; matching its wall time is not a reliable bridge requirement.
