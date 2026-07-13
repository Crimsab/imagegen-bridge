# Command-line interface

The `imagegen-bridge` binary uses the same normalized contract, layered
configuration, provider registry, runtime limits, and artifact verification as
the Rust facade and HTTP server. It never requires an `OPENAI_API_KEY` for the
Codex providers; they inherit the existing Codex OAuth login.

Build the private-development binary with:

```sh
cargo build --release -p imagegen-bridge-cli
./target/release/imagegen-bridge auth-doctor
```

## Generation and editing

Every normalized request field is available either through a complete native
JSON request or ergonomic flags:

```sh
imagegen-bridge generate \
  --prompt "A red origami fox on warm gray" \
  --quality auto \
  --size auto \
  --response-format artifact \
  --filename-prefix fox

imagegen-bridge edit \
  --prompt "Change the jacket to blue" \
  --image ./portrait.png \
  --reference ./palette.png \
  --response-format artifact

imagegen-bridge generate --request request.json --json
imagegen-bridge generate --request - --json < request.json
imagegen-bridge generate --prompt - --dry-run --json < prompt.txt
```

`--request` is lossless and exclusive: it cannot be mixed with prompt, input,
or parameter flags. `--dry-run` validates and prints the normalized request
without opening artifact/session storage or starting Codex.

Advanced flags include `--negative-prompt`, `--negative-prompt-mode`,
`--revised-prompt`, `--aspect-ratio`, `--resolution`, `--compression`,
`--background`, `--moderation`, `--partial-images`, `--compatibility`,
`--session`, `--session-key`, `--thread-id`, `--idempotency-key`, and
`--timeout-ms`. Availability is provider/model-specific; inspect it before
requesting strict parameters:

```sh
imagegen-bridge providers list --json
imagegen-bridge providers capabilities --json
imagegen-bridge providers readiness --json
```

Unsupported strict parameters fail before provider work. They are never
silently discarded. `--compatibility normalize` or `best_effort` permits only
the explicit fallbacks reported in `normalizations`.

## Sessions and server

`--session-key NAME` implies persistent mode and reuses the same app-server
thread on later calls. `--thread-id ID` implies explicit-thread mode.

```sh
imagegen-bridge session get gallery --json
imagegen-bridge session delete gallery --dry-run --json
imagegen-bridge session delete gallery --force --json
imagegen-bridge serve
imagegen-bridge serve --bind 127.0.0.1:9000
```

Session deletion and artifact cleanup require `--force`; their `--dry-run`
forms are non-mutating. `serve` reports its listener on stderr and stops
gracefully on Ctrl-C/SIGINT.

## Configuration, diagnostics, and schemas

Configuration precedence is:

```text
defaults < --config TOML < IMAGEGEN_BRIDGE__* environment < --set/--unset
```

```sh
imagegen-bridge config check --json
imagegen-bridge config show --json
imagegen-bridge config origins --json
imagegen-bridge --set runtime.default_timeout_ms=600000 config check
imagegen-bridge auth-doctor --json
imagegen-bridge auth-doctor --provider codex-app-server --json
imagegen-bridge schema --kind json-schema
imagegen-bridge schema --kind openapi
imagegen-bridge schema --kind openapi --check schemas/imagegen-bridge-v1.openapi.json
imagegen-bridge completions bash
imagegen-bridge man --output imagegen-bridge.1
```

`config check` is non-mutating. `config show` prints credential environment
variable names, never their resolved values. `config origins` prints field
paths and source keys without values. `auth-doctor` performs non-generating
provider checks.

## Output and exit contract

Primary results go to stdout. Diagnostics and server status go to stderr.
`--json` emits stable JSON; `--plain` emits compact line-oriented output;
human output never prints base64 bodies. JSON base64 output is refused when
stdout is an interactive terminal unless `--allow-inline` is explicit.

| Exit | Meaning |
| ---: | --- |
| `0` | Success |
| `2` | CLI syntax, invalid request, configuration, or idempotency conflict |
| `3` | Authentication, permission, or safety rejection |
| `4` | Input, artifact, or session failure |
| `5` | Rate limit or bounded-capacity overload |
| `6` | Unsupported capability, upstream, or protocol failure |
| `70` | Unexpected internal failure |
| `124` | Request deadline exceeded |
| `130` | Cancelled or interrupted operation |

stdin, request files, configuration files, multipart bodies, RPC messages, and
generated artifacts all retain their configured byte/count/time bounds.
