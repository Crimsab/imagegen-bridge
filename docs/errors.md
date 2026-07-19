# Errors and recovery

Every error uses a stable code and includes one or more recovery suggestions.
The same data is available from the CLI, HTTP API, Python SDK, and TypeScript
SDK. Applications can branch on `code`; the suggestions are concise guidance
for people and should not be parsed as protocol fields.

## Structured shape

```json
{
  "error": {
    "code": "overloaded",
    "message": "provider queue is full",
    "retryable": true,
    "suggestions": [
      "Inspect /v1/diagnostics for the saturated admission gate.",
      "Increase the matching max_concurrent/max_queued value or set it to unlimited."
    ]
  }
}
```

Human CLI output prints the same advice as `suggestion:` lines. JSON CLI mode
preserves the array unchanged. SDK exceptions expose it as `suggestions`.

## Common fixes

| Code | Meaning | Typical action |
| --- | --- | --- |
| `overloaded` | A finite bridge admission or queue setting is full | Inspect `/v1/diagnostics`; raise the matching value or use `"unlimited"` |
| `rate_limited` | The upstream Codex service is throttling requests | Honor `Retry-After`; if it repeats, choose a finite provider concurrency |
| `timeout` | The request exceeded a configured deadline | Raise the request or provider timeout and check upstream latency |
| `configuration` | A setting is invalid or internally inconsistent | Run `imagegen-bridge config check` and correct the named field |
| `authentication` | Codex OAuth or bridge authentication is unavailable | Run `imagegen-bridge doctor` and refresh the relevant credentials |
| `upstream` | Codex returned an unavailable or invalid response | Retry when marked retryable; inspect attempt metadata and correlated logs |
| `unsupported` | The selected provider cannot honor a request feature | Inspect `/v1/capabilities` or choose the compatible provider |

## Concurrency is operator policy

The default profile does not impose a bridge-side concurrency or queue cap:

```toml
[runtime.global]
max_concurrent = "unlimited"
max_queued = "unlimited"

[providers.codex-responses]
max_outputs = 255
max_parallel_outputs = "auto"

[server]
max_connections = "unlimited"
```

With `max_parallel_outputs = "auto"`, a request for five distinct images starts
five independent provider calls. To protect a smaller host or accommodate an
upstream rate limit, replace any `"unlimited"` value with a positive integer
and replace `"auto"` with the desired per-request parallelism.

`max_queued = "unlimited"` deliberately allows an unbounded bridge queue when
paired with a finite concurrency cap. That can increase memory use during a
large spike. It is available because capacity policy belongs to the operator;
a finite production queue is still a sensible choice when overload shedding is
preferred.

The protocol count remains an unsigned 8-bit value, so one request can ask for
at most 255 outputs. This is a request-shape boundary, not a global or
cross-client concurrency limit.
