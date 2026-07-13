# HTTP API contract

The API is intended for local tools, agents, SDKs, and authenticated private
network clients. Version 1 evolves additively; breaking wire changes use `/v2`.

## Core endpoints

| Method | Route | Purpose | Success |
| --- | --- | --- | --- |
| `GET` | `/health/live` | Process liveness | `200` |
| `GET` | `/health/ready` | Per-provider readiness | `200` ready, `503` otherwise |
| `GET` | `/v1/providers` | Cursor-paginated provider inventory | `200` |
| `GET` | `/v1/providers/{name}/capabilities` | Dynamic model capabilities | `200` |
| `POST` | `/v1/images` | Lossless normalized generation/edit request | `200` |
| `POST` | `/v1/images/generations` | OpenAI-familiar generation compatibility | `200` |
| `POST` | `/v1/images/edits` | Multipart edit compatibility | `200` |
| `GET` | `/v1/openapi.json` | Checked OpenAPI 3.1 contract | `200` |

Native generation accepts the versioned `ImageRequest` schema and returns
`ImageResponse`. `Idempotency-Key` may be supplied for POST requests. The bridge
returns an `x-request-id` response header for every request.

## Authentication

When `server.bearer_token_env` is configured, `/v1/**` requires
`Authorization: Bearer …`. The token is unrelated to Codex OAuth and is never
included in configuration dumps or diagnostics. Health endpoints remain
available for container orchestration and reveal no secret values.

## Errors

Errors never use a successful HTTP status:

```json
{
  "error": {
    "code": "invalid_request",
    "message": "request validation failed",
    "retryable": false,
    "details": {}
  },
  "request_id": "019f..."
}
```

Validation/input errors map to `400`/`422`, missing authentication to `401`,
permission failures to `403`, conflicts to `409`, rate limits to `429`, capacity
or readiness failures to `503`, deadlines to `504`, and unexpected bridge or
upstream failures to `500`/`502`.

## Provider pagination

`GET /v1/providers?limit=20&cursor=...` accepts `1..=100`. Cursors are opaque and
stable for the immutable provider registry. The response contains `items` and
an optional `next_cursor`.

