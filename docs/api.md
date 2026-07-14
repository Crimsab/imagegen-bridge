# HTTP API contract

The API is intended for local tools, agents, SDKs, and authenticated private
network clients. Version 1 evolves additively; breaking wire changes use `/v2`.

## Core endpoints

| Method | Route | Purpose | Success |
| --- | --- | --- | --- |
| `GET` | `/health/live` | Process liveness | `200` |
| `GET` | `/health/ready` | Per-provider readiness | `200` ready, `503` otherwise |
| `GET` | `/metrics` | Opt-in Prometheus exposition | `200` when enabled |
| `GET` | `/v1/providers` | Cursor-paginated provider inventory | `200` |
| `GET` | `/v1/providers/{name}/capabilities` | Dynamic model capabilities | `200` |
| `GET` | `/v1/diagnostics` | Authenticated redaction-safe operator state | `200` |
| `POST` | `/v1/images` | Lossless normalized generation/edit request | `200` |
| `POST` | `/v1/images/stream` | Bounded native SSE lifecycle | `200` |
| `POST` | `/v1/images/generations` | OpenAI-familiar generation compatibility | `200` |
| `POST` | `/v1/images/edits` | Multipart edit compatibility | `200` |
| `POST` | `/v1/jobs` | Create a durable artifact-backed operation | `202` |
| `GET` | `/v1/jobs` | Cursor-paginated job history | `200` |
| `GET` | `/v1/jobs/{id}` | Complete durable job detail | `200` |
| `DELETE` | `/v1/jobs/{id}` | Request cancellation | `200` |
| `PATCH` | `/v1/jobs/{id}` | Favorite, soft-delete, or restore history | `200` |
| `GET` | `/v1/artifacts/{id}` | Ownership-verified image delivery | `200` |
| `GET` | `/v1/artifacts/{id}/thumbnail` | Bounded PNG thumbnail | `200` |
| `GET` | `/v1/openapi.json` | Checked OpenAPI 3.1 contract | `200` |

Native generation accepts the versioned `ImageRequest` schema and returns
`ImageResponse`. `Idempotency-Key` may be supplied for POST requests. The bridge
returns an `x-request-id` response header for every request.

The SSE handler emits `started`, bounded provider `progress`/`partial_image`
events when available, then `completed` or `error`, with heartbeat comments,
backpressure, and disconnect cancellation.

`GET /v1/diagnostics` returns only aggregate operational facts: bridge version,
listener scope, whether bridge authentication is required, configuration field
origins without values, bounded runtime queue depths, provider readiness, job
status counts and configured retention/admission limits. It never returns
credential values, prompts, account IDs, input data, artifact/database paths, or
job/session identifiers. SQLite storage is reported only as an aggregate byte
count. The route follows the same bridge bearer policy as other `/v1/**`
endpoints.

## Durable jobs and history

`POST /v1/jobs` accepts the native `ImageRequest`, validates it before
persistence, forces `output.response_format=artifact`, and returns an
`ImageJob` with status `queued`. `GET /v1/jobs/{id}` returns the retained
request plus a verified result or structured terminal error. List responses
contain only `ImageJobSummary` records, so inline request images are not copied
into history pages.

The queue is bounded by `server.jobs.max_pending`; workers are independently
bounded by `server.jobs.max_running`. Cancellation is persisted before an
active provider token is signaled. Queued jobs are immediately terminal;
running jobs settle after cooperative cancellation. On restart, queued jobs
resume, while previously running jobs become `interrupted`. They are never
retried automatically because provider completion and billing may be
ambiguous. Retention is bounded by both age and terminal-record count.

`GET /v1/jobs?limit=20&cursor=...&status=succeeded` uses an opaque, stable
newest-first cursor. `limit` is `1..=100`. Optional `visibility` selects
`active` (default), `hidden`, or `all`; `favorite=true|false` and the
case-insensitive literal prompt substring `search` are applied before cursor
pagination. The deprecated `include_deleted=true` alias still means
`visibility=all` for existing clients. The current lifecycle values are
`queued`, `running`, `succeeded`, `failed`, `cancelled`, and `interrupted`.
`PATCH /v1/jobs/{id}` accepts `favorite` and/or `deleted` booleans. Deletion is
soft, terminal-only, hidden from ordinary lists, and reversible. It preserves
the job evidence. Favorite job records are explicitly exempt from automatic
job-history pruning until unfavorited; artifact bytes still follow the separate
artifact-retention policy.

Artifact routes never resolve caller-supplied filenames. They look up the
opaque ID through the bridge ownership record, re-check the checksum and full
image decode, and return only verified PNG/JPEG/WebP bytes. Thumbnail requests
run off the async reactor, accept a `32..=2048` maximum edge, preserve aspect
ratio, and always return a verified PNG with private immutable caching.

## Embedded dashboard

When durable jobs are enabled, `GET /dashboard` serves a static HTML, CSS, and
native JavaScript application embedded in the server binary. It adds no runtime
process, package manager, CDN request, or writable static directory. The UI can
submit generation and edit requests, attach local edit/reference images as data
URLs, discover provider capabilities, poll durable jobs, and manage favorite,
hidden, restored, and cancelled states. Artifact previews are fetched as blobs
through authenticated requests, so bearer tokens never appear in image URLs.
Result details can copy the portable output directory from an artifact name.
This deliberately copies `.` or a relative directory: the API does not expose
the server's configured artifact root or offer a remote file-manager action.

The dashboard shell is intentionally public because browser navigation cannot
attach an Authorization header. It contains no prompt, history, provider result,
credential, or artifact data. Every data API and artifact request remains under
the normal bridge bearer policy. A token entered in the Connection dialog is
stored only in the tab's `sessionStorage`. Responses use a self-only content
security policy, deny framing, disable referrers, and do not permit inline script
or style execution. Disabling `server.jobs.enabled` removes all dashboard routes.

Native multi-image requests accept `parameters.failure_policy` as `fail_fast`
or `best_effort`. Results retain the requested `index` and optional
`generation_ms`; best-effort responses add structured per-index `failures` and
the `partial_output_failure` warning. If every output fails, the request still
returns an error rather than an empty success.

Artifact and URL delivery accept portable `output.directory` and
`output.filename` controls below the server's configured artifact root. An
exact filename requires `parameters.n=1`; its extension may be omitted or must
match `parameters.output_format`. Publication is atomic and never overwrites.
`output.collision` defaults to `error` and may be `suffix` to allocate a
deterministic `-2`, `-3`, … name. The suffix policy is valid only with an exact
filename. Filesystem paths are never accepted by the HTTP contract and absolute
server paths are never returned to clients.

`output.metadata` is `none` by default. `sidecar` is accepted only with
artifact delivery and writes a bounded JSON object next to every image. The
sidecar includes version/request identity, completion timestamp, a path-free
operation summary, original/effective/negative prompts, effective policies,
requested/effective parameters, normalizations, revised prompt, provider/model,
usage, session, timings, warnings, and verified per-image dimensions, format,
byte count and checksum. Its portable relative path is returned in each
image's optional `metadata_name`. Sidecar JSON is independently checksummed in
the ownership record and participates in conservative retention cleanup. It is
an explicit privacy choice: deployments that publicly serve the artifact root
must treat sidecars as equally public.

`parameters.input_fidelity` is optional and accepts `low` or `high` only when
the selected model advertises it. `parameters.action` is `auto`, `generate`, or
`edit`; intrinsic operation conflicts fail before provider work. Provider
capabilities expose the exact accepted fidelity/action sets. The compatible
multipart edit route accepts the official `input_fidelity` field. Masks remain
present in the contract but both Codex providers currently advertise them as
unsupported and reject them before generation.

## Authentication

When `server.bearer_token_env` is configured, `/v1/**` and the opt-in
`/metrics` endpoint require `Authorization: Bearer …`. The token is unrelated
to Codex OAuth and is never included in configuration dumps or diagnostics.
Health endpoints remain available for container orchestration and reveal no
secret values.

## Observability

`server.tracing.enabled` defaults to `true` for the standalone CLI server. It
emits newline-delimited JSON to stderr at INFO level. Image-operation events
contain only the generated request ID, a registered provider name, stable error
code, and retryability; prompts, negative prompts, session keys, account IDs,
paths, image bodies, upstream bodies, and authentication data are never fields.
There is intentionally no supported content-logging switch. A future diagnostic
mode that exposes prompts or image content would require an explicit dangerous
configuration name, warnings, isolation, and separate security review.

`server.metrics.enabled` defaults to `false`. When enabled, `GET /metrics`
exports in-process Prometheus text metrics for request outcomes, operation,
provider and queue time, verified generated bytes, explicit normalizations,
current bounded queue depth, and supervised provider restarts. Labels are
limited to registered provider names, fixed scopes/results, and the bridge's
stable error taxonomy. The endpoint is covered by bridge bearer authentication
when configured and otherwise follows the listener's network trust boundary.

## Errors

Errors never use a successful HTTP status:

```json
{
  "error": {
    "message": "request validation failed",
    "type": "invalid_request_error",
    "param": "prompt",
    "code": "invalid_request",
    "imagegen_bridge": {
      "code": "invalid_request",
      "retryable": false,
      "details": { "field": "prompt" }
    }
  },
  "request_id": "019f..."
}
```

The four standard fields under `error` are consumable by OpenAI clients.
`error.code` is the compatibility discriminator; image safety blocks use
`type=image_generation_user_error` and `code=moderation_blocked`. The
`imagegen_bridge` extension always preserves the original bridge error code,
retryability, safe provider/upstream IDs, and redaction-safe structured detail.
Safety rejections include `safety_category=content_policy`,
`recovery=revise_prompt_or_inputs`, `retry_same_request=false`, the requested
moderation mode when available, and whether input images were present. The
bridge does not automatically weaken the prompt or moderation setting.
When Codex returns public moderation details, the bridge preserves only the
documented `input|output|unknown` stage and coarse allow-listed categories;
unknown/internal classifier labels are discarded.
The top-level request ID also appears in the `x-request-id` response header.

Validation/input errors map to `400`/`422`, missing authentication to `401`,
permission failures to `403`, conflicts to `409`, rate limits to `429`, capacity
or readiness failures to `503`, deadlines to `504`, and unexpected bridge or
upstream failures to `500`/`502`.

The checked-in OpenAPI 3.1 document includes native and compatibility request,
response, error, extension, multipart, provider, session, job, readiness, and SSE
schemas with examples. It is generated from the Rust contract and verified for
drift by CI and `imagegen-bridge schema --kind openapi --check FILE`.

## Provider pagination

`GET /v1/providers?limit=20&cursor=...` accepts `1..=100`. Cursors are opaque and
stable for the immutable provider registry. The response contains `items` and
an optional `next_cursor`. Each descriptor includes a `models` inventory. Query
each entry through `/v1/providers/{name}/capabilities?model=...`; provider/model
differences are authoritative and unsupported models return a structured error.
