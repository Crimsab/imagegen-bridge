# Command-line interface

The `imagegen-bridge` binary uses the same normalized contract, layered
configuration, provider registry, runtime limits, and artifact verification as
the Rust facade and HTTP server. It never requires an `OPENAI_API_KEY` for the
Codex providers; they inherit the existing Codex OAuth login.

Build the binary from a local checkout with:

```sh
cargo build --release -p imagegen-bridge-cli
./target/release/imagegen-bridge setup
./target/release/imagegen-bridge doctor
```

## Guided setup and diagnostics

`setup` is idempotent and safe to rerun. It detects the Codex executable and
ChatGPT OAuth login, chooses XDG-compatible config/state/data locations,
previews all changes, writes configuration atomically, protects the state
directory, and initializes the session database. It does not copy OAuth tokens.

```sh
imagegen-bridge setup --dry-run
imagegen-bridge setup
imagegen-bridge setup --yes --non-interactive --json
imagegen-bridge setup \
  --config ./bridge.toml \
  --state-root ./data/state \
  --output-root ./data/artifacts \
  --yes --non-interactive
```

Interactive mutations require confirmation. Machine output modes never prompt;
use `--yes` to apply their displayed plan. A missing or incomplete installation
can be repaired by rerunning the same command. No paid request occurs unless
`--live-probe` is present, and that probe requires a separate confirmation (or
an explicit `--yes`).

`doctor` checks the bridge version, config file and schema, Codex version,
protected OAuth file, storage permissions, SQLite migration version, configured
port, provider readiness, and capability discovery:

```sh
imagegen-bridge doctor
imagegen-bridge doctor --provider codex-responses --json
imagegen-bridge doctor --live-probe
imagegen-bridge doctor --live-probe --yes --non-interactive --json
```

The normal doctor path is non-generating. The live probe produces exactly one
image, verifies it through the ordinary runtime, returns dimensions/format and
elapsed time, and does not retain image bytes because it requests metadata
output.

## Generation and editing

Every normalized request field is available either through a complete native
JSON request or ergonomic flags:

```sh
imagegen-bridge generate \
  "A red origami fox on warm gray" \
  --quality auto \
  --size auto \
  --response-format artifact \
  --filename-prefix fox

imagegen-bridge edit \
  "Change the jacket to blue" \
  --image ./portrait.png \
  --reference ./palette.png \
  --response-format artifact

imagegen-bridge generate --request request.json --json
imagegen-bridge generate --request - --json < request.json
imagegen-bridge generate --prompt - --dry-run --json < prompt.txt
imagegen-bridge generate "a red-haired woman" --dry-run --json
```

`--request` is lossless and exclusive: it cannot be mixed with prompt, input,
or parameter flags. `--dry-run` validates and prints the normalized request
without opening artifact/session storage or starting Codex.

Advanced flags include `--negative-prompt`, `--negative-prompt-mode`,
`--revised-prompt`, `--aspect-ratio`, `--resolution`, `--compression`,
`--background`, `--moderation`, `--partial-images`, `--failure-policy`,
`--input-fidelity`, `--action`, `--compatibility`,
`--session`, `--session-key`, `--thread-id`, `--idempotency-key`, and
`--timeout-ms`. Availability is provider/model-specific; inspect it before
requesting strict parameters:

```sh
imagegen-bridge providers list --json
imagegen-bridge providers capabilities --json
imagegen-bridge providers readiness --json
```

Image-generation parameters covered by provider capabilities fail before
provider work when strict and unsupported. `--compatibility normalize` or
`best_effort` permits only the explicit fallbacks reported in
`normalizations`. The native contract also accepts an opaque `--user` field.
Both current Codex transports reject it before provider work because neither
upstream path proves that it consumes this attribution; the field is never
silently discarded, including in `best_effort` mode.

`--failure-policy fail_fast` is the default for multiple outputs.
`--failure-policy best_effort` keeps successful outputs and reports failed
indices in `failures`; this is separate from provider compatibility
`--compatibility best_effort`.

`--input-fidelity low|high` requires at least one source/reference image.
`gpt-image-2` accepts only `high` because its inputs are always processed at
high fidelity. `--action edit` likewise requires image context, while
`--action generate` cannot be combined with the native edit operation. Masks
are rejected during capability negotiation on both current Codex transports.

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

When `--config` is omitted, the CLI checks `./imagegen-bridge.toml` first and
then the XDG user configuration (`$XDG_CONFIG_HOME/imagegen-bridge/config.toml`,
falling back to `~/.config/imagegen-bridge/config.toml`).

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
